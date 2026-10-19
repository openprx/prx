#![cfg(test)]
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

mod actions;
mod announce;
mod chat_assign;
mod credential_containment;
mod credential_resolution;
mod join;
mod join_verdicts;
mod kill_and_events;
mod lineage;
mod liveness;
mod owned_child;
mod process_manifest;
mod schema_matrix;
mod spawn;
mod tool_setup;

use super::*;
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::config::MultimodalConfig;
use crate::hooks::HookManager;
use crate::memory::MessageEventScope;
use crate::memory::{Memory, MemoryEventInput, MemoryPrincipal, MemoryVisibility, SqliteMemory};
use crate::observability::NoopObserver;
use crate::security::SecurityPolicy;
use crate::security::policy::ApprovalGrant;
use crate::session_worker::protocol::{WorkerControlFrame, WorkerManifest};
use crate::tools::traits::ToolResult;
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::json;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn test_security() -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy::default())
}

struct DynamicCapabilityTool;

#[async_trait]
impl Tool for DynamicCapabilityTool {
    fn name(&self) -> &str {
        "capability_call"
    }

    fn description(&self) -> &str {
        "dynamic capability fixture"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {"root": {"type": "boolean"}}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: "capability_call".to_string(),
            error: None,
        })
    }

    fn specs(&self) -> Vec<crate::tools::ToolSpec> {
        vec![
            self.spec(),
            crate::tools::ToolSpec {
                name: "capability__dynamic".to_string(),
                description: "dynamic alias".to_string(),
                parameters: json!({"type": "object", "required": ["value"]}),
            },
        ]
    }

    fn supports_name(&self, name: &str) -> bool {
        matches!(name, "capability_call" | "capability__dynamic")
    }

    async fn execute_named(&self, name: &str, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: name.to_string(),
            error: None,
        })
    }
}

struct RefreshableDynamicCapabilityTool {
    refreshed: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Tool for RefreshableDynamicCapabilityTool {
    fn name(&self) -> &str {
        "refreshable_router"
    }

    fn description(&self) -> &str {
        "Dynamic discovery fixture"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    fn specs(&self) -> Vec<crate::tools::ToolSpec> {
        if self.refreshed.load(std::sync::atomic::Ordering::SeqCst) {
            vec![crate::tools::ToolSpec {
                name: "refreshable__alias".to_string(),
                description: "Discovered alias".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        } else {
            vec![self.spec()]
        }
    }

    fn supports_name(&self, name: &str) -> bool {
        name == self.name()
            || (name == "refreshable__alias" && self.refreshed.load(std::sync::atomic::Ordering::SeqCst))
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        self.refreshed.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: self.name().to_string(),
            error: None,
        })
    }

    async fn execute_named(&self, name: &str, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: name.to_string(),
            error: None,
        })
    }
}

/// A channel that records sent messages.
struct RecordingChannel {
    sent: Arc<Mutex<Vec<String>>>,
}

impl RecordingChannel {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        (Self { sent: sent.clone() }, sent)
    }
}

#[async_trait::async_trait]
impl Channel for RecordingChannel {
    fn name(&self) -> &str {
        "recording"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent.lock().await.push(message.content.clone());
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A provider that returns a canned response.
struct EchoProvider {
    response: String,
}

#[async_trait::async_trait]
impl crate::providers::Provider for EchoProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(self.response.clone())
    }

    async fn chat(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        Ok(crate::providers::ChatResponse {
            text: Some(self.response.clone()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    async fn chat_traced(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        let started_at = chrono::Utc::now();
        let finished_at = chrono::Utc::now();
        Ok(crate::providers::traits::ChatTrace {
            response: crate::providers::ChatResponse {
                text: Some(self.response.clone()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            attempts: vec![crate::llm::route_decision::ProviderAttempt {
                seq: 1,
                provider: "ollama".to_string(),
                model: model.to_string(),
                started_at,
                finished_at,
                status: crate::llm::route_decision::AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "ollama".to_string(),
            final_model: model.to_string(),
            served_member: None,
            tokens_used: crate::llm::route_decision::TokenUsage::default(),
        })
    }
}

/// A provider that parks until the test hands out permits, so every run it
/// serves stays `Running` at the same time. That is what lets a fan-out test
/// observe many simultaneously live runs rather than a fast serial trickle.
struct GatedProvider {
    gate: Arc<tokio::sync::Semaphore>,
    entered: Option<Arc<tokio::sync::Notify>>,
}

impl GatedProvider {
    /// Park until the test releases this run.
    async fn wait_for_release(&self) {
        if let Some(entered) = &self.entered {
            entered.notify_one();
        }
        if let Ok(permit) = self.gate.acquire().await {
            drop(permit);
        }
    }
}

#[async_trait::async_trait]
impl crate::providers::Provider for GatedProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.wait_for_release().await;
        Ok("gated".to_string())
    }

    async fn chat(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        self.wait_for_release().await;
        Ok(crate::providers::ChatResponse {
            text: Some("gated".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    async fn chat_traced(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        self.wait_for_release().await;
        let started_at = chrono::Utc::now();
        let finished_at = chrono::Utc::now();
        Ok(crate::providers::traits::ChatTrace {
            response: crate::providers::ChatResponse {
                text: Some("gated".to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            attempts: vec![crate::llm::route_decision::ProviderAttempt {
                seq: 1,
                provider: "gated".to_string(),
                model: model.to_string(),
                started_at,
                finished_at,
                status: crate::llm::route_decision::AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "gated".to_string(),
            final_model: model.to_string(),
            served_member: None,
            tokens_used: crate::llm::route_decision::TokenUsage::default(),
        })
    }
}

struct TaskTraceParentProvider;

#[async_trait::async_trait]
impl Provider for TaskTraceParentProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("task trace fixture requires the shared parent loop")
    }
    async fn chat(
        &self,
        request: crate::providers::ChatRequest<'_>,
        _: &str,
        _: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        let completed = request.messages.iter().any(|message| {
            message.role == "tool"
                || (message.role == "user"
                    && message
                        .content
                        .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX))
        });
        let content = if completed {
            "parent complete".to_owned()
        } else {
            format!(
                "<tool_call>\n{}\n</tool_call>",
                json!({"name":"sessions_spawn",
                "arguments":{"task":"metered task", "mode":"task"}})
            )
        };
        Ok(crate::providers::ChatResponse {
            text: Some(content),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

struct TaskTraceServer(tokio::task::JoinHandle<()>);
impl Drop for TaskTraceServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A provider that always fails.
struct FailingProvider;

#[async_trait::async_trait]
impl crate::providers::Provider for FailingProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(anyhow!("provider failure"))
    }

    async fn chat(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        Err(anyhow!("provider failure"))
    }
}

/// A provider that sleeps before responding, so a spawned run stays in the
/// `Running` state long enough for a kill test to act on it deterministically.
struct SleepyProvider {
    delay_ms: u64,
    response: String,
}

#[async_trait::async_trait]
impl crate::providers::Provider for SleepyProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(self.response.clone())
    }

    async fn chat(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(crate::providers::ChatResponse {
            text: Some(self.response.clone()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    async fn chat_traced(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        let started_at = chrono::Utc::now();
        let finished_at = chrono::Utc::now();
        Ok(crate::providers::traits::ChatTrace {
            response: crate::providers::ChatResponse {
                text: Some(self.response.clone()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            attempts: vec![crate::llm::route_decision::ProviderAttempt {
                seq: 1,
                provider: "sleepy".to_string(),
                model: model.to_string(),
                started_at,
                finished_at,
                status: crate::llm::route_decision::AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "sleepy".to_string(),
            final_model: model.to_string(),
            served_member: None,
            tokens_used: crate::llm::route_decision::TokenUsage::default(),
        })
    }
}

struct EchoSystemProvider;

#[async_trait::async_trait]
impl crate::providers::Provider for EchoSystemProvider {
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(system.unwrap_or_default().to_string())
    }

    async fn chat(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        Ok(crate::providers::ChatResponse {
            text: Some(String::new()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    async fn chat_traced(
        &self,
        request: crate::providers::ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        let system = request
            .messages
            .iter()
            .find(|message| message.role == "system")
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let started_at = chrono::Utc::now();
        let finished_at = chrono::Utc::now();
        Ok(crate::providers::traits::ChatTrace {
            response: crate::providers::ChatResponse {
                text: Some(system),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            attempts: vec![crate::llm::route_decision::ProviderAttempt {
                seq: 1,
                provider: "echo-system".to_string(),
                model: model.to_string(),
                started_at,
                finished_at,
                status: crate::llm::route_decision::AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "echo-system".to_string(),
            final_model: model.to_string(),
            served_member: None,
            tokens_used: crate::llm::route_decision::TokenUsage::default(),
        })
    }
}

fn make_agent_config(identity_dir: Option<String>) -> DelegateAgentConfig {
    DelegateAgentConfig {
        provider: "ollama".to_string(),
        model: "prx-local-model".to_string(),
        system_prompt: None,
        api_key: None,
        credential_ref: None,
        temperature: None,
        max_depth: 3,
        agentic: false,
        allowed_tools: Vec::new(),
        identity_dir,
        memory_scope: None,
        spawn_enabled: None,
    }
}

/// Q71: derive only the approved parent metadata from the original fixture.
fn preconstruction_parent_config(tool: &SessionsSpawnTool, workspace: &std::path::Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config.config_path = workspace.join("config.toml");
    config.default_provider = Some(tool.provider_name.clone());
    config.default_model = Some(tool.model.clone());
    config.agents = tool.agents.as_ref().clone();
    config.providers.models.push(
        serde_json::from_value(json!({
            "provider": tool.provider_name, "model": tool.model,
            "operations": ["chat"], "native_tools": true
        }))
        .expect("test: approved parent model metadata"),
    );
    config.cost.prices.insert(
        format!("{}/{}", tool.provider_name, tool.model),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config
}

/// Dispatch through the existing Q68 real Chat/default-Builtin source seam.
/// Child arguments, security, provider Arc and per-case configuration remain original.
async fn dispatch_preconstruction_parent(
    tool: SessionsSpawnTool,
    config: crate::config::Config,
    memory: Arc<dyn Memory>,
    arguments: serde_json::Value,
) -> anyhow::Result<ToolResult> {
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())?;
    cleanup
        .run(Box::pin(async move {
            let provider = Arc::clone(tool.test_parent_provider());
            let security = Arc::clone(&tool.security);
            let workspace = config.workspace_dir.clone();
            // Q71 hybrid is deliberately invalid for persistence; publish the
            // original in-memory fixture so only actual dispatch can reject it.
            assert!(!config.config_path.exists());
            let generation = crate::config::new_shared(config).pin();
            let tools_handle = tool.tools_handle();
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(tool)]);
            assert!(tools_handle.set(&registry));
            crate::chat::decision_source::admission_failure_tests::dispatch_unknown_agent_fixture(
                crate::runtime::turn_publication::TurnDecisionPublication {
                    generation,
                    registry,
                    mcp_tool: None,
                    security,
                    observer: Arc::new(NoopObserver),
                    hooks: Arc::new(HookManager::new(workspace)),
                    memory,
                    plugin_generation: 0,
                },
                provider,
                arguments,
            )
            .await
        }))
        .await
}

fn make_tool(channel: Arc<dyn Channel>, provider: Arc<dyn crate::providers::Provider>) -> SessionsSpawnTool {
    make_tool_with_security_and_spawn_config(
        channel,
        provider,
        test_security(),
        crate::config::SessionsSpawnConfig::default(),
    )
}

fn make_tool_with_spawn_config(
    channel: Arc<dyn Channel>,
    provider: Arc<dyn crate::providers::Provider>,
    spawn_config: crate::config::SessionsSpawnConfig,
) -> SessionsSpawnTool {
    make_tool_with_security_and_spawn_config(channel, provider, test_security(), spawn_config)
}

fn make_tool_with_security_and_spawn_config(
    channel: Arc<dyn Channel>,
    provider: Arc<dyn crate::providers::Provider>,
    security: Arc<SecurityPolicy>,
    spawn_config: crate::config::SessionsSpawnConfig,
) -> SessionsSpawnTool {
    SessionsSpawnTool::new(
        channel,
        "ollama",
        "prx-local-model",
        0.7,
        security,
        std::path::PathBuf::from("/tmp"),
        crate::config::MultimodalConfig::default(),
        HashMap::new(),
        None,
        crate::providers::ProviderRuntimeOptions::default(),
        spawn_config,
    )
    .with_test_parent_provider(provider)
}

/// FIX-P0-37: spawning is now a Medium-risk side effect, which requires an
/// approval grant under explicit supervised autonomy. Tests that drive
/// the real `spawn` path must inject a matching grant — mirroring how the
/// production agent loop issues one after operator approval. The operation
/// name MUST equal the one the gate authorizes (`sessions_spawn:spawn`).
fn spawn_grant_value() -> serde_json::Value {
    serde_json::to_value(ApprovalGrant::for_resource_operation(
        "sessions_spawn",
        "sessions_spawn:spawn",
        "test",
        None,
    ))
    .unwrap()
}

/// Merge a valid spawn approval grant into the given `spawn` arguments so the
/// Medium-risk gate authorizes the call.
fn with_spawn_grant(mut args: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = args.as_object_mut() {
        obj.insert(
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
            spawn_grant_value(),
        );
    }
    args
}

/// Clone the immutable tool definition while sharing every live runtime
/// handle. The admission service owns this copy for one dispatch; assertions
/// keep using the original handle to observe the same runs, recipients,
/// channels, batch rosters, and child registry.
fn admission_clone(tool: &SessionsSpawnTool) -> SessionsSpawnTool {
    SessionsSpawnTool {
        channel: Arc::clone(&tool.channel),
        test_worker_executable: tool.test_worker_executable.clone(),
        channels: Arc::clone(&tool.channels),
        test_parent_provider: tool.test_parent_provider.clone(),
        provider_name: tool.provider_name.clone(),
        model: tool.model.clone(),
        security: Arc::clone(&tool.security),
        default_recipient: Arc::clone(&tool.default_recipient),
        active_runs: Arc::clone(&tool.active_runs),
        tools: Arc::clone(&tool.tools),
        workspace_dir: tool.workspace_dir.clone(),
        agents: Arc::clone(&tool.agents),
        fallback_api_key: tool.fallback_api_key.clone(),
        configured_providers: tool.configured_providers.clone(),
        model_catalog: Arc::clone(&tool.model_catalog),
        hint_config: tool.hint_config.clone(),
        default_route_source: tool.default_route_source.clone(),
        fallback_api_key_owner: tool.fallback_api_key_owner.clone(),
        fallback_uses_root_connection: tool.fallback_uses_root_connection,
        cost_config: tool.cost_config.clone(),
        memory: tool.memory.clone(),
        source_fabric: tool.source_fabric.clone(),
        // Recovery is owned by the original production-like fixture. A
        // dispatch copy must not create a second recovery owner.
        restart_recovery: None,
        event_recording: tool.event_recording.clone(),
        event_sink: tool.event_sink.clone(),
        approval_resolver_factory: tool.approval_resolver_factory.clone(),
        batch_rosters: Arc::clone(&tool.batch_rosters),
    }
}

/// Admit a recorded parent and invoke `sessions_spawn` through the
/// production decision/execution binding. Every real model caller reaches
/// the tool this way; a raw unit call has neither a parent grant nor an
/// ordinary publication and must fail closed.
async fn execute_admitted_spawn(tool: &SessionsSpawnTool, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
    execute_admitted_spawn_with_plane(tool, arguments, false).await
}

async fn execute_enabled_admitted_spawn(
    tool: &SessionsSpawnTool,
    arguments: serde_json::Value,
) -> anyhow::Result<ToolResult> {
    execute_admitted_spawn_with_plane(tool, arguments, true).await
}

async fn execute_admitted_spawn_with_plane(
    tool: &SessionsSpawnTool,
    arguments: serde_json::Value,
    decision_enabled: bool,
) -> anyhow::Result<ToolResult> {
    fn fixture_price() -> crate::config::schema::ModelPricing {
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        }
    }

    static WORKSPACES: std::sync::OnceLock<parking_lot::Mutex<Vec<tempfile::TempDir>>> = std::sync::OnceLock::new();
    let trusted_scope = arguments
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .filter(|trusted| *trusted)
        .and_then(|_| arguments.get("_zc_scope"))
        .and_then(serde_json::Value::as_object);
    let field = |key: &str| {
        trusted_scope
            .and_then(|scope| scope.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let workspace = field("workspace_id")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| tool.workspace_dir.clone());
    let admission_memory: Arc<dyn Memory> = match &tool.memory {
        Some(memory) => Arc::clone(memory),
        None => {
            let admission_workspace = tempfile::tempdir()?;
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(admission_workspace.path())?);
            WORKSPACES
                .get_or_init(|| parking_lot::Mutex::new(Vec::new()))
                .lock()
                .push(admission_workspace);
            memory
        }
    };
    let mut config = tool
        .hint_config
        .as_deref()
        .cloned()
        .unwrap_or_else(crate::config::Config::default);
    config.workspace_dir = workspace.clone();
    config.config_path = workspace.join("config.toml");
    config.default_provider = Some(tool.provider_name.clone());
    config.default_model = Some(tool.model.clone());
    config.causal_tree.enabled = decision_enabled;
    config.autonomy.level = tool.security.autonomy;
    config.autonomy.workspace_only = tool.security.workspace_only;
    config
        .autonomy
        .forbidden_paths
        .clone_from(&tool.security.forbidden_paths);
    config.autonomy.max_cost_per_day_cents = tool.security.max_cost_per_day_cents;
    config.autonomy.scopes.rules.clone_from(&tool.security.scope_rules);
    config.autonomy.scopes.default = if tool.security.scope_default_allow {
        crate::config::ScopeDefault::Allow
    } else {
        crate::config::ScopeDefault::Deny
    };
    config
        .autonomy
        .scopes
        .assign_owners
        .clone_from(&tool.security.assign_owners);
    config.security.audit = tool.security.audit_config.clone();
    config.agents = tool.agents.as_ref().clone();
    config.cost = tool.cost_config.clone();
    if !config
        .providers
        .models
        .iter()
        .any(|model| model.provider == tool.provider_name && model.model.as_deref() == Some(tool.model.as_str()))
    {
        config.providers.models.push(serde_json::from_value(json!({
            "provider": tool.provider_name,
            "model": tool.model,
            "operations": ["chat"],
            "native_tools": true
        }))?);
    }
    for route in config.model_routes.clone() {
        if !config
            .providers
            .models
            .iter()
            .any(|model| model.provider == route.provider && model.model.as_deref() == Some(route.model.as_str()))
        {
            config.providers.models.push(serde_json::from_value(json!({
                "provider": route.provider,
                "model": route.model,
                "operations": ["chat"],
                "native_tools": true
            }))?);
        }
        config
            .cost
            .prices
            .entry(format!("{}/{}", route.provider, route.model))
            .or_insert_with(fixture_price);
    }
    for agent in config.agents.values() {
        if !config
            .providers
            .models
            .iter()
            .any(|model| model.provider == agent.provider && model.model.as_deref() == Some(agent.model.as_str()))
        {
            config.providers.models.push(serde_json::from_value(json!({
                "provider": agent.provider,
                "model": agent.model,
                "operations": ["chat"],
                "native_tools": true
            }))?);
        }
        config
            .cost
            .prices
            .entry(format!("{}/{}", agent.provider, agent.model))
            .or_insert_with(fixture_price);
    }
    config
        .cost
        .prices
        .entry(format!("{}/{}", tool.provider_name, tool.model))
        .or_insert_with(fixture_price);
    config
        .cost
        .prices
        .entry("ollama/prx-local-model".to_owned())
        .or_insert_with(fixture_price);
    let shared: Arc<dyn Tool> = Arc::new(admission_clone(tool));
    let provider = Arc::clone(tool.test_parent_provider());
    let provider_name = tool.provider_name.clone();
    let model_name = tool.model.clone();
    let registry =
        crate::agent::test_admission::registry_with_search(vec![crate::tools::ArcDelegatingTool::boxed(shared)]);
    let chat_kind = match field("chat_type") {
        Some("group") => crate::channels::traits::ChatKind::Group,
        Some("thread") => crate::channels::traits::ChatKind::Thread,
        _ => crate::channels::traits::ChatKind::Dm,
    };
    let message = ChannelMessage {
        id: field("source_message_event_id")
            .unwrap_or("prx-sessions-spawn-parent")
            .to_owned(),
        sender: field("sender").unwrap_or("prx_user").to_owned(),
        reply_target: field("chat_id").unwrap_or_default().to_owned(),
        content: if decision_enabled {
            arguments
                .get("task")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("complete the requested child task")
                .to_owned()
        } else {
            "sessions_spawn admitted fixture".to_owned()
        },
        channel: field("channel").unwrap_or("wacli").to_owned(),
        timestamp: 0,
        chat_kind,
        chat_title: None,
        sender_display: None,
        thread_ts: field("topic_id").map(str::to_owned),
        mentioned_uuids: Vec::new(),
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())?;
    cleanup
        .run(Box::pin(async move {
            let admitted = if decision_enabled {
                crate::agent::test_admission::AdmittedTurn::enabled_builtin_channel_with_memory(
                    config,
                    message,
                    registry,
                    admission_memory,
                    (&provider_name, &model_name),
                )
                .await
            } else {
                crate::agent::test_admission::AdmittedTurn::ordinary_controlled_channel_with_memory(
                    config,
                    message,
                    registry,
                    admission_memory,
                    (&provider_name, &model_name),
                )
                .await
            };
            crate::runtime::child_source::scope_ordinary_provider_fixture(
                provider,
                provider_name.clone(),
                model_name.clone(),
                admitted.execute_tool("sessions_spawn", arguments),
            )
            .await
        }))
        .await
}

/// A channel that records both its name and the messages it sent, so a test
/// can assert *which* channel a sub-agent result was announced on.
struct NamedRecordingChannel {
    name: &'static str,
    sent: Arc<Mutex<Vec<String>>>,
}

impl NamedRecordingChannel {
    fn new(name: &'static str) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                name,
                sent: sent.clone(),
            }),
            sent,
        )
    }
}

#[async_trait::async_trait]
impl Channel for NamedRecordingChannel {
    fn name(&self) -> &str {
        self.name
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent.lock().await.push(message.content.clone());
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Build a tool whose announce/kill routing registry knows several named
/// channels, so a test can assert a run resolves the *originating* channel by
/// name from its per-turn scope rather than from shared "active" state.
fn make_tool_with_channels(
    default_channel: Arc<dyn Channel>,
    provider: Arc<dyn crate::providers::Provider>,
    registry: Vec<Arc<dyn Channel>>,
) -> SessionsSpawnTool {
    let channels: HashMap<String, Arc<dyn Channel>> =
        registry.into_iter().map(|ch| (ch.name().to_string(), ch)).collect();
    make_tool(default_channel, provider).with_channels(Arc::new(channels))
}

/// Build a trusted per-turn spawn scope arg pinning the originating channel
/// and chat_id (recipient) — mirrors the `_zc_scope` the agent loop injects
/// for the message currently being processed.
fn with_scope(mut args: serde_json::Value, channel: &str, chat_id: &str) -> serde_json::Value {
    if let Some(obj) = args.as_object_mut() {
        obj.insert("_zc_scope_trusted".to_string(), json!(true));
        obj.insert(
            "_zc_scope".to_string(),
            // `workspace_id` as `normalize_arguments` always authors it:
            // the run owner (OWNER-VIS) is composed from it on both the
            // spawn and the kill/steer side.
            json!({
                "sender": "alice",
                "channel": channel,
                "chat_type": "group",
                "chat_id": chat_id,
                "workspace_id": "/tmp",
            }),
        );
    }
    args
}

fn make_tool_with_channels_and_security(
    default_channel: Arc<dyn Channel>,
    provider: Arc<dyn crate::providers::Provider>,
    registry: Vec<Arc<dyn Channel>>,
    security: Arc<SecurityPolicy>,
) -> SessionsSpawnTool {
    let channels: HashMap<String, Arc<dyn Channel>> =
        registry.into_iter().map(|ch| (ch.name().to_string(), ch)).collect();
    make_tool_with_security_and_spawn_config(
        default_channel,
        provider,
        security,
        crate::config::SessionsSpawnConfig::default(),
    )
    .with_channels(Arc::new(channels))
}

/// Build a bare `SubAgentRun` with the given id and status for unit-testing
/// the deterministic approval-suspension restore logic.
fn restore_test_run(id: &str, status: SubAgentStatus) -> SubAgentRun {
    SubAgentRun {
        progress: crate::agent::idle::child_beat(),
        batch_id: None,
        id: id.to_string(),
        task: "t".to_string(),
        owner_id: None,
        topic_id: None,
        source_message_event_id: None,
        started_at: Utc::now(),
        finished_at: None,
        status,
        recipient: None,
        channel_name: None,
        abort_handle: None,
        process_control: None,
        task_control: None,
        history: Arc::new(RwLock::new(Vec::new())),
        steer_tx: None,
        parent_run_id: None,
        session_scope_key: "s".to_string(),
        spawn_depth: 0,
        delegation: None,
        token_usage_records: Vec::new(),
    }
}

/// Bounded wait for a run to leave `Running`, so a propagation test fails
/// loudly instead of hanging when the propagation is removed.
async fn await_terminal_reason(active_runs: &Arc<RwLock<Vec<SubAgentRun>>>, run_id: &str) -> String {
    for _ in 0..400 {
        {
            let runs = active_runs.read().await;
            if let Some(run) = runs.iter().find(|run| run.id == run_id) {
                match &run.status {
                    SubAgentStatus::Failed(reason) => return reason.clone(),
                    SubAgentStatus::Completed(output) => return output.clone(),
                    SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => {}
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("run '{run_id}' never reached a terminal status: it is still reported as running");
}

fn parse_json(output: &str) -> serde_json::Value {
    serde_json::from_str(output).unwrap_or_else(|error| panic!("tool output must be JSON: {error}\n{output}"))
}

fn batch_run_ids(payload: &serde_json::Value) -> Vec<String> {
    payload["spawned"]
        .as_array()
        .expect("spawn_batch reports what it started")
        .iter()
        .map(|entry| {
            entry["run_id"]
                .as_str()
                .expect("every started member has a run id")
                .to_string()
        })
        .collect()
}
