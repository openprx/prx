#![allow(clippy::print_stdout, clippy::print_stderr)]

#[path = "embedded_turn.rs"]
mod embedded_turn;

use crate::agent::dispatcher::ToolDispatcher;
#[cfg(test)]
use crate::agent::dispatcher::{ParsedToolCall, ToolExecutionResult};
use crate::agent::memory_loader::{MemoryLoader, SelectedMemoryFormatter};
use crate::agent::prompt::SystemPromptBuilder;
use crate::hooks::HookManager;
#[cfg(test)]
use crate::hooks::{HookEvent, payload_error};
use crate::memory::Memory;
#[cfg(test)]
use crate::memory::MemoryCategory;
use crate::observability::Observer;
#[cfg(test)]
use crate::observability::ObserverEvent;
#[cfg(test)]
use crate::providers::ChatRequest;
use crate::providers::{ConversationMessage, Provider};
#[cfg(feature = "llm-router")]
use crate::router::RouterEngine;
use crate::runtime::control_ladder::ControlLadderSnapshot;
use crate::tools::Tool;
use anyhow::Result;
use std::sync::Arc;
#[cfg(test)]
use std::time::Instant;

pub struct Agent {
    provider: Arc<dyn Provider>,
    embedded_binding: Option<Arc<super::embedded_source::EmbeddedAgentBinding>>,
    runtime_generation: Option<Arc<crate::config::ConfigGeneration>>,
    cleanup_state: embedded_turn::CleanupState,
    security: Arc<crate::security::SecurityPolicy>,
    mcp_tool: Option<Arc<crate::tools::McpTool>>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    /// The one gate every tool call on this shell passes through: autonomy
    /// policy, approval, published-schema validation and the audit record.
    #[cfg(test)]
    tool_execution: Arc<crate::tools::ToolExecutionService>,
    /// Session key stamped on this agent's runtime envelope, minted once so a
    /// whole conversation audits under one key.
    #[cfg(test)]
    session_key: String,
    memory: Arc<dyn Memory>,
    observer: Arc<dyn Observer>,
    hooks: Arc<HookManager>,
    prompt_builder: SystemPromptBuilder,
    tool_dispatcher: Box<dyn ToolDispatcher>,
    selected_memory_formatter: Option<Box<dyn SelectedMemoryFormatter>>,
    memory_loader: Option<Arc<dyn MemoryLoader>>,
    _config: crate::config::AgentConfig,
    model_name: String,
    temperature: f64,
    workspace_dir: std::path::PathBuf,
    identity_config: crate::config::IdentityConfig,
    history: Vec<ConversationMessage>,
    #[cfg(feature = "llm-router")]
    router: Option<Arc<RouterEngine>>,
}

pub struct AgentBuilder {
    provider: Option<Box<dyn Provider>>,
    runtime_config: Option<(crate::config::Config, String)>,
    route_providers: Vec<super::embedded_source::EmbeddedProviderInput>,
    mcp_tool: Option<Arc<crate::tools::McpTool>>,
    tools: Option<Vec<Box<dyn Tool>>>,
    memory: Option<Arc<dyn Memory>>,
    observer: Option<Arc<dyn Observer>>,
    hooks: Option<Arc<HookManager>>,
    prompt_builder: Option<SystemPromptBuilder>,
    tool_dispatcher: Option<Box<dyn ToolDispatcher>>,
    memory_loader: Option<Box<dyn MemoryLoader>>,
    selected_memory_formatter: Option<Box<dyn SelectedMemoryFormatter>>,
    config: Option<crate::config::AgentConfig>,
    model_name: Option<String>,
    temperature: Option<f64>,
    workspace_dir: Option<std::path::PathBuf>,
    identity_config: Option<crate::config::IdentityConfig>,
    skills: Option<Vec<crate::skills::Skill>>,
    auto_save: Option<bool>,
    classification_config: Option<crate::config::QueryClassificationConfig>,
    task_routing_config: Option<crate::config::TaskRoutingConfig>,
    tool_tiering: Option<crate::config::ToolTieringConfig>,
    available_hints: Option<Vec<String>>,
    control_ladder: Option<ControlLadderSnapshot>,
    security_policy: Option<Arc<crate::security::SecurityPolicy>>,
    #[cfg(feature = "llm-router")]
    model_routes: Option<Vec<crate::config::ModelRouteConfig>>,
    #[cfg(feature = "llm-router")]
    router: Option<RouterEngine>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            provider: None,
            runtime_config: None,
            route_providers: Vec::new(),
            mcp_tool: None,
            tools: None,
            memory: None,
            observer: None,
            hooks: None,
            prompt_builder: None,
            tool_dispatcher: None,
            memory_loader: None,
            selected_memory_formatter: None,
            config: None,
            model_name: None,
            temperature: None,
            workspace_dir: None,
            identity_config: None,
            skills: None,
            auto_save: None,
            classification_config: None,
            task_routing_config: None,
            tool_tiering: None,
            available_hints: None,
            control_ladder: None,
            security_policy: None,
            #[cfg(feature = "llm-router")]
            model_routes: None,
            #[cfg(feature = "llm-router")]
            router: None,
        }
    }

    pub fn provider(mut self, provider: Box<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Supply the complete runtime configuration and the exact provider label
    /// for an embedded Agent. Explicit builder overrides are applied before
    /// the generation is pinned; the supplied provider instance is retained.
    pub fn runtime_config(mut self, config: crate::config::Config, provider_name: impl Into<String>) -> Self {
        self.runtime_config = Some((config, provider_name.into()));
        self
    }

    /// Retain the same MCP metadata owner used by the registered tool backend.
    pub fn mcp_tool(mut self, tool: Arc<crate::tools::McpTool>) -> Self {
        self.mcp_tool = Some(tool);
        self
    }

    pub fn tools(mut self, tools: Vec<Box<dyn Tool>>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Bind the autonomy/scope policy the execution service enforces for this
    /// agent's tool calls. Defaults to the policy the stock autonomy
    /// configuration produces for the agent's workspace.
    pub fn security_policy(mut self, policy: Arc<crate::security::SecurityPolicy>) -> Self {
        self.security_policy = Some(policy);
        self
    }

    pub fn memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn hooks(mut self, hooks: Arc<HookManager>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    pub fn prompt_builder(mut self, prompt_builder: SystemPromptBuilder) -> Self {
        self.prompt_builder = Some(prompt_builder);
        self
    }

    pub fn tool_dispatcher(mut self, tool_dispatcher: Box<dyn ToolDispatcher>) -> Self {
        self.tool_dispatcher = Some(tool_dispatcher);
        self
    }

    /// Bind an additional actual provider instance for configured model routes.
    /// The declared selection must resolve to this exact provider connection.
    pub fn route_provider(mut self, provider_name: String, model_name: String, provider: Box<dyn Provider>) -> Self {
        self.route_providers
            .push(super::embedded_source::EmbeddedProviderInput {
                provider_name,
                model_name,
                provider: Arc::from(provider),
            });
        self
    }

    /// Run custom retrieval only after the source-bound decision selects Memory.
    /// The callback receives a read-only view supporting bounded recall; writes,
    /// key/list reads require migration. Explicit session filters must match
    /// the source session. Recall returns stored ids/keys/categories/timestamps;
    /// semantic scores and ancillary metadata are unavailable in this path.
    pub fn memory_loader(mut self, memory_loader: Box<dyn MemoryLoader>) -> Self {
        self.memory_loader = Some(memory_loader);
        self
    }

    /// Customize the display of decision-selected memory without opening a
    /// second retrieval path. Not invoked when no memory evidence is selected.
    pub fn selected_memory_formatter(mut self, formatter: Box<dyn SelectedMemoryFormatter>) -> Self {
        self.selected_memory_formatter = Some(formatter);
        self
    }

    pub fn config(mut self, config: crate::config::AgentConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn model_name(mut self, model_name: String) -> Self {
        self.model_name = Some(model_name);
        self
    }

    pub const fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn workspace_dir(mut self, workspace_dir: std::path::PathBuf) -> Self {
        self.workspace_dir = Some(workspace_dir);
        self
    }

    pub fn identity_config(mut self, identity_config: crate::config::IdentityConfig) -> Self {
        self.identity_config = Some(identity_config);
        self
    }

    pub fn skills(mut self, skills: Vec<crate::skills::Skill>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub const fn auto_save(mut self, auto_save: bool) -> Self {
        self.auto_save = Some(auto_save);
        self
    }

    pub fn classification_config(mut self, classification_config: crate::config::QueryClassificationConfig) -> Self {
        self.classification_config = Some(classification_config);
        self
    }

    pub fn available_hints(mut self, available_hints: Vec<String>) -> Self {
        self.available_hints = Some(available_hints);
        self
    }

    pub fn control_ladder(mut self, control_ladder: ControlLadderSnapshot) -> Self {
        self.control_ladder = Some(control_ladder);
        self
    }

    #[cfg(feature = "llm-router")]
    pub fn model_routes(mut self, model_routes: Vec<crate::config::ModelRouteConfig>) -> Self {
        self.model_routes = Some(model_routes);
        self
    }

    pub fn task_routing_config(mut self, task_routing_config: crate::config::TaskRoutingConfig) -> Self {
        self.task_routing_config = Some(task_routing_config);
        self
    }

    pub fn tool_tiering(mut self, tool_tiering: crate::config::ToolTieringConfig) -> Self {
        self.tool_tiering = Some(tool_tiering);
        self
    }

    #[cfg(feature = "llm-router")]
    pub fn router(mut self, router: RouterEngine) -> Self {
        self.router = Some(router);
        self
    }

    fn pin_runtime_config(&mut self) -> Result<Option<(Arc<crate::config::ConfigGeneration>, String)>> {
        let Some((mut config, provider_name)) = self.runtime_config.take() else {
            return Ok(None);
        };
        anyhow::ensure!(
            !provider_name.trim().is_empty(),
            "embedded Agent requires an explicit provider label"
        );
        config.workspace_dir = self
            .workspace_dir
            .get_or_insert_with(|| config.workspace_dir.clone())
            .clone();
        config.agent = self.config.get_or_insert_with(|| config.agent.clone()).clone();
        config.identity = self
            .identity_config
            .get_or_insert_with(|| config.identity.clone())
            .clone();
        config.tool_tiering = self
            .tool_tiering
            .get_or_insert_with(|| config.tool_tiering.clone())
            .clone();
        config.query_classification = self
            .classification_config
            .get_or_insert_with(|| config.query_classification.clone())
            .clone();
        config.task_routing = self
            .task_routing_config
            .get_or_insert_with(|| config.task_routing.clone())
            .clone();
        config.default_temperature = *self.temperature.get_or_insert(config.default_temperature);
        config.memory.auto_save = *self.auto_save.get_or_insert(config.memory.auto_save);
        let model = self
            .model_name
            .get_or_insert_with(|| crate::providers::selection::default_model(&config).to_owned())
            .clone();
        config.default_model = Some(model);
        config.default_provider = Some(provider_name.clone());
        #[cfg(feature = "llm-router")]
        {
            config.model_routes = self
                .model_routes
                .get_or_insert_with(|| config.model_routes.clone())
                .clone();
        }
        Ok(Some((crate::config::new_shared(config).pin(), provider_name)))
    }

    pub fn build(mut self) -> Result<Agent> {
        // Legacy callbacks cannot run outside the source-bound decision graph,
        // and accepting them without invoking them silently changes behavior.
        // Reject before pinning configuration or constructing runtime services.
        anyhow::ensure!(
            self.memory_loader.is_none() || self.runtime_config.is_some(),
            "embedded Agent custom MemoryLoader requires runtime_config for selected retrieval"
        );
        if self.runtime_config.is_some() {
            anyhow::ensure!(
                self.skills.as_ref().is_none_or(Vec::is_empty),
                "embedded Agent skills must come from the workspace capability catalog; injected skill bodies cannot bypass selected skill_read"
            );
        }
        anyhow::ensure!(
            self.runtime_config.is_some() || self.route_providers.is_empty(),
            "embedded Agent route providers require runtime_config"
        );
        let runtime = self.pin_runtime_config()?;
        if runtime.is_some() {
            crate::runtime::decision_accounting::initialize_tokenizers();
        }
        let mut tools = self.tools.ok_or_else(|| anyhow::anyhow!("tools are required"))?;
        if runtime.is_some() && !tools.iter().any(|tool| tool.name() == "capability_search") {
            tools.push(Box::new(crate::tools::capability::search_tool::CapabilitySearchTool));
        }
        let tools = Arc::new(tools);
        let workspace_dir = self.workspace_dir.unwrap_or_else(|| std::path::PathBuf::from("."));
        let hooks = self
            .hooks
            .unwrap_or_else(|| Arc::new(HookManager::new(workspace_dir.clone())));
        // Every tool call this shell makes goes through the same service the
        // agent loop, Chat and the gateway use, so one contract is enforced in
        // one place. Approval is fail-closed: this shell has no way to ask a
        // human, so a tool the autonomy level wants confirmed is refused rather
        // than auto-approved.
        let policy = self.security_policy.unwrap_or_else(|| {
            Arc::new(crate::security::SecurityPolicy::from_config(
                &runtime
                    .as_ref()
                    .map(|(generation, _)| generation.effective.autonomy.clone())
                    .unwrap_or_default(),
                &workspace_dir,
            ))
        });
        #[cfg(test)]
        let tool_execution = Arc::new(crate::tools::ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&tools),
            Arc::new(crate::tools::SecurityEffectPolicy::new(Arc::clone(&policy))),
            Arc::new(crate::tools::DenyApprovalStrategy),
            Arc::new(crate::tools::AdapterOwnedPreparation),
            Arc::new(crate::tools::TracingToolExecutionAudit),
        ));

        let provider: Arc<dyn Provider> =
            Arc::from(self.provider.ok_or_else(|| anyhow::anyhow!("provider is required"))?);
        let memory = self.memory.ok_or_else(|| anyhow::anyhow!("memory is required"))?;
        #[cfg(feature = "llm-router")]
        if let Some(router) = &self.router {
            let (generation, provider_name) = runtime
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("embedded Router requires runtime_config"))?;
            router.validate_embedded_binding(&generation.effective, provider_name, &memory)?;
        }
        let embedded_binding = runtime
            .as_ref()
            .map(|(generation, provider_name)| {
                anyhow::ensure!(
                    policy.workspace_dir == workspace_dir,
                    "embedded Agent security workspace differs from its pinned configuration"
                );
                super::embedded_source::EmbeddedAgentBinding::capture_with_providers(
                    Arc::clone(generation),
                    Arc::clone(&provider),
                    (
                        provider_name,
                        self.model_name
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("embedded Agent runtime model is missing"))?,
                    ),
                    Arc::clone(&memory),
                    std::mem::take(&mut self.route_providers),
                )
            })
            .transpose()?;
        // Legacy adapter hints are accepted for API compatibility. Shared turns
        // obtain routes and control policy from the pinned runtime generation.
        drop(self.available_hints);
        drop(self.control_ladder);
        Ok(Agent {
            provider,
            embedded_binding,
            runtime_generation: runtime.map(|(generation, _)| generation),
            cleanup_state: embedded_turn::CleanupState::default(),
            security: policy,
            mcp_tool: self.mcp_tool,
            tools,
            #[cfg(test)]
            tool_execution,
            #[cfg(test)]
            session_key: format!("agent:{}", uuid::Uuid::now_v7()),
            memory,
            observer: self.observer.ok_or_else(|| anyhow::anyhow!("observer is required"))?,
            hooks,
            prompt_builder: self.prompt_builder.unwrap_or_else(SystemPromptBuilder::with_defaults),
            tool_dispatcher: self
                .tool_dispatcher
                .ok_or_else(|| anyhow::anyhow!("tool_dispatcher is required"))?,
            selected_memory_formatter: self.selected_memory_formatter,
            memory_loader: self.memory_loader.map(Arc::from),
            _config: self.config.unwrap_or_default(),
            model_name: self
                .model_name
                .unwrap_or_else(|| crate::providers::selection::default_model_for_unset().to_owned()),
            temperature: self.temperature.unwrap_or(0.7),
            workspace_dir,
            identity_config: self.identity_config.unwrap_or_default(),
            history: Vec::new(),
            #[cfg(feature = "llm-router")]
            router: self.router.map(Arc::new),
        })
    }
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub fn history(&self) -> &[ConversationMessage] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// The runtime envelope this shell stamps on every tool call.
    ///
    /// The scope is deliberately *not* the local-operator tuple: nobody is at a
    /// keyboard answering for this path, so `is_local_operator` stays false and
    /// the destructive-action guard keeps refusing.
    /// The execution context for tool calls answered by `model`: arguments are
    /// validated in the dialect of the model the turn actually sent to
    /// (after classification, routing or escalation), not the configured one.
    #[cfg(test)]
    fn tool_execution_context(&self, model: &str) -> crate::tools::ToolExecutionContext {
        let envelope = crate::runtime::envelope::RuntimeEnvelope::chat_terminal(
            self.workspace_dir.to_string_lossy().to_string(),
            self.session_key.clone(),
            crate::memory::MemoryVisibility::Workspace,
        )
        .with_sender("agent")
        .with_channel("agent");
        crate::tools::ToolExecutionContext::new(envelope, "private")
            .with_chat_id("agent:local")
            .with_schema_dialect(self.provider.tool_schema_dialect_for(model))
    }

    /// Run one parsed tool call through [`crate::tools::ToolExecutionService`].
    ///
    /// Until 0.8.128 this called `Tool::execute_named` directly, which meant a
    /// model-authored argument document reached a tool with no autonomy policy
    /// decision, no approval gate, no published-schema check and no audit
    /// record — a second execution path with its own rules. It now shares the
    /// single pipeline, so the contract a tool advertises is the contract it is
    /// held to on every entry point.
    #[cfg(test)]
    async fn execute_tool_call(
        &self,
        call: &ParsedToolCall,
        compiled_tools: &crate::agent::turn_context::CompiledToolContext,
        model: &str,
    ) -> ToolExecutionResult {
        let start = Instant::now();
        self.hooks
            .emit(
                HookEvent::ToolCallStart,
                serde_json::json!({
                    "tool": call.name,
                    "arguments": call.arguments,
                }),
            )
            .await;

        if !self.tools.iter().any(|tool| tool.supports_name(&call.name)) {
            let available_names: Vec<&str> = self.tools.iter().map(|tool| tool.name()).collect();
            let suggestion = crate::tools::error_hints::suggest_tool_name(&call.name, &available_names);
            let hint = suggestion
                .map(|suggestion| format!(" Did you mean '{suggestion}'?"))
                .unwrap_or_default();
            let message = format!(
                "Error: unknown tool '{}'.{hint}\nAvailable tools: {}",
                call.name,
                available_names.join(", ")
            );
            self.hooks.emit(HookEvent::Error, payload_error("tool", &message)).await;
            return ToolExecutionResult {
                name: call.name.clone(),
                output: message,
                success: false,
                tool_call_id: call.tool_call_id.clone(),
            };
        }

        if !compiled_tools.allows(&call.name) {
            return ToolExecutionResult {
                name: call.name.clone(),
                output: format!(
                    "Error: tool '{}' was not exposed for this turn and was not executed",
                    call.name
                ),
                success: false,
                tool_call_id: call.tool_call_id.clone(),
            };
        }

        // Runtime-only markers supplied by the model are dropped and re-authored
        // by the service from the context above, so a forged `_zc_scope` or
        // approval grant in the argument document cannot survive into execution.
        let command = crate::tools::ToolExecutionCommand::new(call.name.clone(), call.arguments.clone());
        let outcome = self
            .tool_execution
            .execute(command, self.tool_execution_context(model), None)
            .await;
        let success = outcome.succeeded();
        let output = match outcome.result {
            Some(result) if result.success => result.output,
            Some(result) => {
                let error_text = result.error.unwrap_or(result.output);
                let hint = crate::tools::error_hints::recovery_hint(&call.name, &error_text);
                if hint.is_empty() {
                    format!("Error: {error_text}")
                } else {
                    format!("Error: {error_text}\n{hint}")
                }
            }
            // A refusal by the service (policy, approval, schema, unknown tool)
            // already carries its own model-facing text.
            None => {
                let hint = crate::tools::error_hints::recovery_hint(
                    &call.name,
                    outcome.error.as_deref().unwrap_or(&outcome.model_content),
                );
                if hint.is_empty() {
                    outcome.model_content
                } else {
                    format!("{}\n{hint}", outcome.model_content)
                }
            }
        };
        if success {
            self.hooks
                .emit(
                    HookEvent::ToolCall,
                    serde_json::json!({
                        "tool": call.name,
                        "duration_ms": start.elapsed().as_millis(),
                        "success": true,
                        "error": serde_json::Value::Null,
                        "output": output,
                    }),
                )
                .await;
        } else {
            self.hooks.emit(HookEvent::Error, payload_error("tool", &output)).await;
        }
        self.observer.record_event(&ObserverEvent::ToolCall {
            tool: call.name.clone(),
            duration: start.elapsed(),
            success,
        });

        ToolExecutionResult {
            name: call.name.clone(),
            output,
            success,
            tool_call_id: call.tool_call_id.clone(),
        }
    }

    // Forward the existing future without another generator/poll frame. All
    // admission, execution and cleanup still begin only when it is polled.
    pub fn turn(&mut self, user_message: &str) -> impl std::future::Future<Output = Result<String>> {
        embedded_turn::run(self, user_message)
    }

    pub fn run_single(&mut self, message: &str) -> impl std::future::Future<Output = Result<String>> {
        self.turn(message)
    }
}

/// The `RouteDecision` a legacy agent turn stamps on its trace and persists
/// (FIX-P1-13). The target is `<provider>/<model>`, and a `custom:` provider
/// label may carry `user:password@`, so it is redacted before the split
/// (INT-2B B4f): the persisted decision never holds the credential. The split
/// at the first `/` is unchanged (it still mis-splits a `custom:` URL; the
/// decision is a join record, not an address).
#[cfg(test)]
fn turn_route_decision(effective_model: &str, turn_id: &str) -> crate::llm::route_decision::RouteDecision {
    let target = crate::providers::redact_endpoint_userinfo(effective_model);
    let (provider, model) = target.split_once('/').unwrap_or(("default", &*target));
    crate::llm::route_decision::RouteDecision::single_candidate_for_context(
        provider,
        model,
        "owner:agent",
        format!("agent:{turn_id}"),
        None,
        None,
        "agent.turn",
        0,
        true,
        false,
    )
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "llm-router"))]
mod retired_escalation_tests;

#[cfg(test)]
pub(crate) mod native_decision_fixture;

#[cfg(all(test, unix))]
#[path = "agent/cleanup_diagnostic_tests.rs"]
mod cleanup_diagnostic_tests;
