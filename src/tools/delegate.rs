use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::agent::loop_::{DocumentIngestRuntime, ScopeContext, ToolConcurrencyGovernanceConfig, run_tool_call_loop};
use crate::config::DelegateAgentConfig;
use crate::hooks::HookManager;
use crate::memory::{Memory, MemoryEventRecording, MemoryFabric, MessageEventScope};
use crate::observability::traits::{Observer, ObserverEvent, ObserverMetric};
use crate::providers::{self, ChatMessage, Provider};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::SecurityPolicy;
use crate::security::policy::ToolOperation;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Default timeout for sub-agent provider calls.
const DELEGATE_TIMEOUT_SECS: u64 = 120;
/// Default timeout for agentic sub-agent runs.
const DELEGATE_AGENTIC_TIMEOUT_SECS: u64 = 300;

/// Tool that delegates a subtask to a named agent with a different
/// provider/model configuration. Enables multi-agent workflows where
/// a primary agent can hand off specialized work (research, coding,
/// summarization) to purpose-built sub-agents.
pub struct DelegateTool {
    agents: Arc<HashMap<String, DelegateAgentConfig>>,
    security: Arc<SecurityPolicy>,
    /// Global credential fallback (from config.api_key)
    fallback_credential: Option<String>,
    /// Provider runtime options inherited from root config.
    provider_runtime_options: providers::ProviderRuntimeOptions,
    /// Depth at which this tool instance lives in the delegation chain.
    depth: u32,
    /// Parent tool registry for agentic sub-agents.
    parent_tools: Arc<Vec<Arc<dyn Tool>>>,
    /// Inherited multimodal handling config for sub-agent loops.
    multimodal_config: crate::config::MultimodalConfig,
    /// Compaction config inherited from root agent config.
    compaction_config: crate::config::AgentCompactionConfig,
    /// Shared memory fabric backend for normalized delegation events.
    memory: Option<Arc<dyn Memory>>,
    event_recording: MemoryEventRecording,
}

#[derive(Debug, Clone)]
struct DelegateScope {
    sender: String,
    channel: String,
    chat_type: String,
    chat_id: String,
    owner_id: Option<String>,
    topic_id: Option<String>,
    task_id: Option<String>,
    source_message_event_id: Option<String>,
}

fn parse_delegate_scope(args: &serde_json::Value) -> Option<DelegateScope> {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return None;
    }

    let scope = args.get("_zc_scope").and_then(serde_json::Value::as_object)?;
    let sender = scope
        .get("sender")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let channel = scope
        .get("channel")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let chat_type = scope
        .get("chat_type")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let chat_id = scope
        .get("chat_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();

    Some(DelegateScope {
        sender,
        channel,
        chat_type,
        chat_id,
        owner_id: scope
            .get("owner_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        topic_id: scope
            .get("topic_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        task_id: scope
            .get("task_id")
            .or_else(|| scope.get("parent_task_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        source_message_event_id: scope
            .get("message_event_id")
            .or_else(|| scope.get("source_message_event_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

impl DelegateTool {
    pub fn new(
        agents: HashMap<String, DelegateAgentConfig>,
        fallback_credential: Option<String>,
        security: Arc<SecurityPolicy>,
    ) -> Self {
        Self::new_with_options(
            agents,
            fallback_credential,
            security,
            providers::ProviderRuntimeOptions::default(),
        )
    }

    pub fn new_with_options(
        agents: HashMap<String, DelegateAgentConfig>,
        fallback_credential: Option<String>,
        security: Arc<SecurityPolicy>,
        provider_runtime_options: providers::ProviderRuntimeOptions,
    ) -> Self {
        Self {
            agents: Arc::new(agents),
            security,
            fallback_credential,
            provider_runtime_options,
            depth: 0,
            parent_tools: Arc::new(Vec::new()),
            multimodal_config: crate::config::MultimodalConfig::default(),
            compaction_config: crate::config::AgentCompactionConfig::default(),
            memory: None,
            event_recording: MemoryEventRecording::default(),
        }
    }

    /// Create a DelegateTool for a sub-agent (with incremented depth).
    /// When sub-agents eventually get their own tool registry, construct
    /// their DelegateTool via this method with `depth: parent.depth + 1`.
    pub fn with_depth(
        agents: HashMap<String, DelegateAgentConfig>,
        fallback_credential: Option<String>,
        security: Arc<SecurityPolicy>,
        depth: u32,
    ) -> Self {
        Self::with_depth_and_options(
            agents,
            fallback_credential,
            security,
            depth,
            providers::ProviderRuntimeOptions::default(),
        )
    }

    pub fn with_depth_and_options(
        agents: HashMap<String, DelegateAgentConfig>,
        fallback_credential: Option<String>,
        security: Arc<SecurityPolicy>,
        depth: u32,
        provider_runtime_options: providers::ProviderRuntimeOptions,
    ) -> Self {
        Self {
            agents: Arc::new(agents),
            security,
            fallback_credential,
            provider_runtime_options,
            depth,
            parent_tools: Arc::new(Vec::new()),
            multimodal_config: crate::config::MultimodalConfig::default(),
            compaction_config: crate::config::AgentCompactionConfig::default(),
            memory: None,
            event_recording: MemoryEventRecording::default(),
        }
    }

    /// Attach parent tools used to build sub-agent allowlist registries.
    pub fn with_parent_tools(mut self, parent_tools: Arc<Vec<Arc<dyn Tool>>>) -> Self {
        self.parent_tools = parent_tools;
        self
    }

    /// Attach multimodal configuration for sub-agent tool loops.
    pub const fn with_multimodal_config(mut self, config: crate::config::MultimodalConfig) -> Self {
        self.multimodal_config = config;
        self
    }

    /// Attach compaction configuration for agentic sub-agent loops.
    pub const fn with_compaction_config(mut self, config: crate::config::AgentCompactionConfig) -> Self {
        self.compaction_config = config;
        self
    }

    /// Attach shared memory so delegate requests/results join the live fabric.
    pub fn with_shared_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub const fn with_event_recording(mut self, event_recording: MemoryEventRecording) -> Self {
        self.event_recording = event_recording;
        self
    }
}

fn delegate_session_key(scope: Option<&DelegateScope>) -> String {
    scope
        .map(|scope| format!("delegate:{}:{}:{}", scope.channel, scope.chat_id, scope.sender))
        .unwrap_or_else(|| "delegate:global".to_string())
}

fn delegate_event_scope(
    workspace_id: &str,
    delegate_run_id: &str,
    agent_name: &str,
    scope: Option<&DelegateScope>,
) -> MessageEventScope {
    let mut envelope = RuntimeEnvelope::delegate(workspace_id, delegate_session_key(scope), delegate_run_id)
        .with_channel(scope.map_or("delegate", |scope| scope.channel.as_str()))
        .with_agent_id(agent_name);
    if let Some(scope) = scope {
        envelope = envelope
            .with_sender(scope.sender.as_str())
            .with_recipient(scope.chat_id.as_str());
        if let Some(owner_id) = &scope.owner_id {
            envelope = envelope.with_owner_id(owner_id.clone());
        }
        if let Some(topic_id) = &scope.topic_id {
            envelope = envelope.with_topic_id(topic_id.clone());
        }
        if let Some(task_id) = &scope.task_id {
            envelope = envelope.with_task_id(task_id.clone());
        }
        if let Some(source_message_event_id) = &scope.source_message_event_id {
            envelope = envelope.with_source_message_event_id(source_message_event_id.clone());
        }
    }
    envelope.message_scope()
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Delegate a subtask to a specialized agent. Use when: a task benefits from a different model \
         (e.g. fast summarization, deep reasoning, code generation). The sub-agent runs a single \
         prompt by default; with agentic=true it can iterate with a filtered tool-call loop."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let agent_names: Vec<&str> = self.agents.keys().map(|s: &String| s.as_str()).collect();
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "agent": {
                    "type": "string",
                    "minLength": 1,
                    "description": format!(
                        "Name of the agent to delegate to. Available: {}",
                        if agent_names.is_empty() {
                            "(none configured)".to_string()
                        } else {
                            agent_names.join(", ")
                        }
                    )
                },
                "prompt": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The task/prompt to send to the sub-agent"
                },
                "context": {
                    "type": "string",
                    "description": "Optional context to prepend (e.g. relevant code, prior findings)"
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override. Defaults to the named agent's configured model."
                },
                "provider": {
                    "type": "string",
                    "description": "Optional provider override (e.g. 'openrouter', 'ollama'). Defaults to the named agent's configured provider."
                }
            },
            "required": ["agent", "prompt"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let scope = parse_delegate_scope(&args);
        let agent_name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("Missing 'agent' parameter"))?;

        if agent_name.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'agent' parameter must not be empty".into()),
            });
        }

        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("Missing 'prompt' parameter"))?;

        if prompt.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'prompt' parameter must not be empty".into()),
            });
        }

        let context = args
            .get("context")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");

        // Inline overrides (BUG-10): caller may pin provider/model per call.
        // Priority: inline arg > named agent config > (no global default here;
        // the named agent config always supplies a concrete provider/model).
        let provider_override = args
            .get("provider")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let model_override = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // Look up agent config
        let agent_config = match self.agents.get(agent_name) {
            Some(cfg) => cfg,
            None => {
                let available: Vec<&str> = self.agents.keys().map(|s: &String| s.as_str()).collect();
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Unknown agent '{agent_name}'. Available agents: {}",
                        if available.is_empty() {
                            "(none configured)".to_string()
                        } else {
                            available.join(", ")
                        }
                    )),
                });
            }
        };

        // Check recursion depth (immutable — set at construction, incremented for sub-agents)
        if self.depth >= agent_config.max_depth {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Delegation depth limit reached ({depth}/{max}). \
                     Cannot delegate further to prevent infinite loops.",
                    depth = self.depth,
                    max = agent_config.max_depth
                )),
            });
        }

        if let Err(error) = self.security.enforce_tool_operation(ToolOperation::Act, "delegate") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        // Resolve effective provider/model: inline override > agent config.
        // When omitted, fall back to the agent config (backward compatible).
        let effective_provider = provider_override
            .clone()
            .unwrap_or_else(|| agent_config.provider.clone());
        let effective_model = model_override.clone().unwrap_or_else(|| agent_config.model.clone());

        // Create provider for this agent
        let provider_credential_owned = agent_config
            .api_key
            .clone()
            .or_else(|| self.fallback_credential.clone());
        #[allow(clippy::option_as_ref_deref)]
        let provider_credential = provider_credential_owned.as_ref().map(String::as_str);

        let provider: Box<dyn Provider> = match providers::create_provider_with_options(
            &effective_provider,
            provider_credential,
            &self.provider_runtime_options,
        ) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Failed to create provider '{effective_provider}' for agent '{agent_name}': {e}"
                    )),
                });
            }
        };

        // Build the message
        let full_prompt = if context.is_empty() {
            prompt.to_string()
        } else {
            format!("[Context]\n{context}\n\n[Task]\n{prompt}")
        };

        let temperature = agent_config.temperature.unwrap_or(0.7);
        let delegate_run_id = uuid::Uuid::new_v4().to_string();
        let memory_fabric = self.memory.as_ref().map(|memory| {
            MemoryFabric::new(memory.clone(), self.security.workspace_dir.to_string_lossy())
                .with_event_recording(self.event_recording)
        });
        let event_scope = delegate_event_scope(
            &self.security.workspace_dir.to_string_lossy(),
            &delegate_run_id,
            agent_name,
            scope.as_ref(),
        );
        if let Some(fabric) = memory_fabric.as_ref() {
            if let Err(error) = fabric
                .record_inbound_user_message(
                    event_scope.clone(),
                    prompt,
                    Some(format!("delegate:{delegate_run_id}:request")),
                    Some(
                        json!({
                            "agent": agent_name,
                            "provider": effective_provider,
                            "model": effective_model,
                            "agentic": agent_config.agentic,
                            "has_context": !context.is_empty()
                        })
                        .to_string(),
                    ),
                )
                .await
            {
                tracing::warn!(agent = agent_name, "failed to record delegate request event: {error}");
            }
            record_delegate_task_event(
                fabric,
                event_scope.clone(),
                "delegate.task.started",
                json!({
                    "agent": agent_name,
                    "provider": effective_provider,
                    "model": effective_model,
                    "agentic": agent_config.agentic,
                    "has_context": !context.is_empty(),
                    "owner_id": scope.as_ref().and_then(|scope| scope.owner_id.clone()),
                    "topic_id": scope.as_ref().and_then(|scope| scope.topic_id.clone()),
                    "parent_task_id": scope.as_ref().and_then(|scope| scope.task_id.clone()),
                    "source_message_event_id": scope.as_ref().and_then(|scope| scope.source_message_event_id.clone())
                }),
            )
            .await;
        }

        // Agentic mode: run full tool-call loop with allowlisted tools.
        if agent_config.agentic {
            let result = self
                .execute_agentic(
                    agent_name,
                    agent_config,
                    &effective_provider,
                    &effective_model,
                    &*provider,
                    &full_prompt,
                    temperature,
                    scope.as_ref(),
                )
                .await;
            if let Some(fabric) = memory_fabric.as_ref() {
                record_delegate_result_event(fabric, event_scope.clone(), &result).await;
                record_delegate_terminal_task_event(
                    fabric,
                    event_scope,
                    agent_name,
                    &effective_provider,
                    &effective_model,
                    agent_config.agentic,
                    scope.as_ref(),
                    &result,
                    false,
                )
                .await;
            }
            return result;
        }

        // Wrap the provider call in a timeout to prevent indefinite blocking
        let result = tokio::time::timeout(
            Duration::from_secs(DELEGATE_TIMEOUT_SECS),
            provider.chat_with_system(
                agent_config.system_prompt.as_deref(),
                &full_prompt,
                &effective_model,
                temperature,
            ),
        )
        .await;

        let result = match result {
            Ok(inner) => inner,
            Err(_elapsed) => {
                let tool_result = ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Agent '{agent_name}' timed out after {DELEGATE_TIMEOUT_SECS}s")),
                };
                if let Some(fabric) = memory_fabric.as_ref() {
                    record_delegate_result_event(fabric, event_scope.clone(), &Ok(tool_result.clone())).await;
                    record_delegate_terminal_task_event(
                        fabric,
                        event_scope,
                        agent_name,
                        &effective_provider,
                        &effective_model,
                        agent_config.agentic,
                        scope.as_ref(),
                        &Ok(tool_result.clone()),
                        true,
                    )
                    .await;
                }
                return Ok(tool_result);
            }
        };

        match result {
            Ok(response) => {
                let mut rendered = response;
                if rendered.trim().is_empty() {
                    rendered = "[Empty response]".to_string();
                }

                let tool_result = ToolResult {
                    success: true,
                    output: format!(
                        "[Agent '{agent_name}' ({provider}/{model})]\n{rendered}",
                        provider = effective_provider,
                        model = effective_model
                    ),
                    error: None,
                };
                if let Some(fabric) = memory_fabric.as_ref() {
                    record_delegate_result_event(fabric, event_scope.clone(), &Ok(tool_result.clone())).await;
                    record_delegate_terminal_task_event(
                        fabric,
                        event_scope,
                        agent_name,
                        &effective_provider,
                        &effective_model,
                        agent_config.agentic,
                        scope.as_ref(),
                        &Ok(tool_result.clone()),
                        false,
                    )
                    .await;
                }
                Ok(tool_result)
            }
            Err(e) => {
                let tool_result = ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Agent '{agent_name}' failed: {e}",)),
                };
                if let Some(fabric) = memory_fabric.as_ref() {
                    record_delegate_result_event(fabric, event_scope.clone(), &Ok(tool_result.clone())).await;
                    record_delegate_terminal_task_event(
                        fabric,
                        event_scope,
                        agent_name,
                        &effective_provider,
                        &effective_model,
                        agent_config.agentic,
                        scope.as_ref(),
                        &Ok(tool_result.clone()),
                        false,
                    )
                    .await;
                }
                Ok(tool_result)
            }
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }
}

async fn record_delegate_result_event(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    result: &anyhow::Result<ToolResult>,
) {
    let (success, content, error) = match result {
        Ok(result) => (
            result.success,
            if result.output.trim().is_empty() {
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "[delegate produced no output]".to_string())
            } else {
                result.output.clone()
            },
            result.error.clone(),
        ),
        Err(error) => (false, String::new(), Some(error.to_string())),
    };
    if let Err(error) = fabric
        .record_worker_result(
            scope,
            content,
            Some(json!({ "success": success, "error": error }).to_string()),
        )
        .await
    {
        tracing::warn!("failed to record delegate result event: {error}");
    }
}

async fn record_delegate_task_event(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    event_type: &str,
    payload: serde_json::Value,
) {
    let task_id = scope.run_id.clone().unwrap_or_else(|| "delegate:unknown".to_string());
    if let Err(error) = fabric
        .record_task_event(scope, task_id, event_type.to_string(), Some(payload.to_string()))
        .await
    {
        tracing::warn!(event_type, "failed to record delegate task event: {error}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_delegate_terminal_task_event(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    agent_name: &str,
    provider: &str,
    model: &str,
    agentic: bool,
    delegate_scope: Option<&DelegateScope>,
    result: &anyhow::Result<ToolResult>,
    timeout: bool,
) {
    let (success, error, output_preview) = match result {
        Ok(result) => (
            result.success,
            result.error.clone(),
            result.output.chars().take(500).collect::<String>(),
        ),
        Err(error) => (false, Some(error.to_string()), String::new()),
    };
    let event_type = if timeout {
        "delegate.task.timeout"
    } else if success {
        "delegate.task.completed"
    } else {
        "delegate.task.failed"
    };
    record_delegate_task_event(
        fabric,
        scope,
        event_type,
        json!({
            "success": success,
            "error": error,
            "output_preview": output_preview,
            "agent": agent_name,
            "provider": provider,
            "model": model,
            "agentic": agentic,
            "owner_id": delegate_scope.and_then(|scope| scope.owner_id.clone()),
            "topic_id": delegate_scope.and_then(|scope| scope.topic_id.clone()),
            "parent_task_id": delegate_scope.and_then(|scope| scope.task_id.clone()),
            "source_message_event_id": delegate_scope.and_then(|scope| scope.source_message_event_id.clone())
        }),
    )
    .await;
}

impl DelegateTool {
    #[allow(clippy::too_many_arguments)]
    async fn execute_agentic(
        &self,
        agent_name: &str,
        agent_config: &DelegateAgentConfig,
        effective_provider: &str,
        effective_model: &str,
        provider: &dyn Provider,
        full_prompt: &str,
        temperature: f64,
        scope: Option<&DelegateScope>,
    ) -> anyhow::Result<ToolResult> {
        if agent_config.allowed_tools.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Agent '{agent_name}' has agentic=true but allowed_tools is empty"
                )),
            });
        }

        let allowed = agent_config
            .allowed_tools
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .collect::<std::collections::HashSet<_>>();

        let sub_tools: Vec<Box<dyn Tool>> = self
            .parent_tools
            .iter()
            .filter(|tool| allowed.contains(tool.name()))
            .filter(|tool| tool.name() != "delegate")
            .map(|tool| Box::new(ToolArcRef::new(tool.clone())) as Box<dyn Tool>)
            .collect();

        if sub_tools.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Agent '{agent_name}' has no executable tools after filtering allowlist ({})",
                    agent_config.allowed_tools.join(", ")
                )),
            });
        }

        let mut history = Vec::new();
        if let Some(system_prompt) = agent_config.system_prompt.as_ref() {
            history.push(ChatMessage::system(system_prompt.clone()));
        }
        history.push(ChatMessage::user(full_prompt.to_string()));

        let noop_observer = NoopObserver;
        let hooks = HookManager::new(self.security.workspace_dir.clone());
        let scope_ctx = scope.map(|scope| ScopeContext {
            policy: &self.security,
            sender: scope.sender.as_str(),
            channel: scope.channel.as_str(),
            chat_type: scope.chat_type.as_str(),
            chat_id: scope.chat_id.as_str(),
            owner_id: scope.owner_id.as_deref(),
            topic_id: scope.topic_id.as_deref(),
            task_id: scope.task_id.as_deref(),
            source_message_event_id: scope.source_message_event_id.as_deref(),
            policy_pipeline: None,
        });

        let result = tokio::time::timeout(
            Duration::from_secs(DELEGATE_AGENTIC_TIMEOUT_SECS),
            run_tool_call_loop(
                provider,
                &mut history,
                &sub_tools,
                &noop_observer,
                &hooks,
                effective_provider,
                effective_model,
                temperature,
                true,
                None,
                "delegate",
                &self.multimodal_config,
                agent_config.max_iterations,
                true,
                2,
                30,
                false,
                vec![
                    "sessions_spawn".to_string(),
                    "delegate".to_string(),
                    "cron_run".to_string(),
                ],
                ToolConcurrencyGovernanceConfig {
                    rollout_stage: "full".to_string(),
                    ..ToolConcurrencyGovernanceConfig::default()
                },
                Some(&self.compaction_config),
                None,
                None,
                scope_ctx.as_ref(),
                None,
                None, // delegate sub-agents do not use tool tiering
                scope_ctx.as_ref().and_then(|ctx| {
                    self.memory
                        .as_ref()
                        .map(|memory| DocumentIngestRuntime::from_scope(memory.clone(), ctx))
                }),
                crate::agent::loop_::ChatMode::default(),
            ),
        )
        .await;

        match result {
            Ok(Ok(response)) => {
                let rendered = if response.trim().is_empty() {
                    "[Empty response]".to_string()
                } else {
                    response
                };

                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "[Agent '{agent_name}' ({provider}/{model}, agentic)]\n{rendered}",
                        provider = effective_provider,
                        model = effective_model
                    ),
                    error: None,
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Agent '{agent_name}' failed: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Agent '{agent_name}' timed out after {DELEGATE_AGENTIC_TIMEOUT_SECS}s"
                )),
            }),
        }
    }
}

struct ToolArcRef {
    inner: Arc<dyn Tool>,
}

impl ToolArcRef {
    fn new(inner: Arc<dyn Tool>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Tool for ToolArcRef {
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
}

struct NoopObserver;

impl Observer for NoopObserver {
    fn record_event(&self, _event: &ObserverEvent) {}

    fn record_metric(&self, _metric: &ObserverMetric) {}

    fn name(&self) -> &str {
        "noop"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryPrincipal, SqliteMemory};
    use crate::providers::{ChatRequest, ChatResponse, ToolCall};
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use anyhow::anyhow;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    fn sample_agents() -> HashMap<String, DelegateAgentConfig> {
        let mut agents = HashMap::new();
        agents.insert(
            "researcher".to_string(),
            DelegateAgentConfig {
                provider: "ollama".to_string(),
                model: "llama3".to_string(),
                system_prompt: Some("You are a research assistant.".to_string()),
                api_key: None,
                temperature: Some(0.3),
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 10,
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );
        agents.insert(
            "coder".to_string(),
            DelegateAgentConfig {
                provider: "openrouter".to_string(),
                model: "anthropic/claude-sonnet-4-20250514".to_string(),
                system_prompt: None,
                api_key: Some("delegate-test-credential".to_string()),
                temperature: None,
                max_depth: 2,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 10,
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );
        agents
    }

    #[derive(Default)]
    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }

        fn description(&self) -> &str {
            "Echoes the `value` argument."
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": {"type": "string"}
                },
                "required": ["value"]
            })
        }

        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            let value = args
                .get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(ToolResult {
                success: true,
                output: format!("echo:{value}"),
                error: None,
            })
        }
    }

    struct OneToolThenFinalProvider;

    #[async_trait]
    impl Provider for OneToolThenFinalProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("unused".to_string())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let has_tool_message = request.messages.iter().any(|m| m.role == "tool");
            if has_tool_message {
                Ok(ChatResponse {
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            } else {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "echo_tool".to_string(),
                        arguments: "{\"value\":\"ping\"}".to_string(),
                    }],
                    reasoning_content: None,
                })
            }
        }
    }

    struct InfiniteToolCallProvider;

    #[async_trait]
    impl Provider for InfiniteToolCallProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("unused".to_string())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "loop".to_string(),
                    name: "echo_tool".to_string(),
                    arguments: "{\"value\":\"x\"}".to_string(),
                }],
                reasoning_content: None,
            })
        }
    }

    struct FailingProvider;

    #[async_trait]
    impl Provider for FailingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("unused".to_string())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            Err(anyhow!("provider boom"))
        }
    }

    fn agentic_config(allowed_tools: Vec<String>, max_iterations: usize) -> DelegateAgentConfig {
        DelegateAgentConfig {
            provider: "openrouter".to_string(),
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: None,
            model: "model-test".to_string(),
            system_prompt: Some("You are agentic.".to_string()),
            api_key: Some("delegate-test-credential".to_string()),
            temperature: Some(0.2),
            max_depth: 3,
            agentic: true,
            allowed_tools,
            max_iterations,
        }
    }

    #[test]
    fn name_and_schema() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        assert_eq!(tool.name(), "delegate");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["agent"].is_object());
        assert!(schema["properties"]["prompt"].is_object());
        assert!(schema["properties"]["context"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("agent")));
        assert!(required.contains(&json!("prompt")));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["properties"]["agent"]["minLength"], json!(1));
        assert_eq!(schema["properties"]["prompt"]["minLength"], json!(1));
    }

    #[test]
    fn description_not_empty() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn schema_lists_agent_names() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        let schema = tool.parameters_schema();
        let desc = schema["properties"]["agent"]["description"].as_str().unwrap();
        assert!(desc.contains("researcher") || desc.contains("coder"));
    }

    #[tokio::test]
    async fn missing_agent_param() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        let result = tool.execute(json!({"prompt": "test"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_prompt_param() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        let result = tool.execute(json!({"agent": "researcher"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unknown_agent_returns_error() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        let result = tool
            .execute(json!({"agent": "nonexistent", "prompt": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown agent"));
    }

    #[tokio::test]
    async fn depth_limit_enforced() {
        let tool = DelegateTool::with_depth(sample_agents(), None, test_security(), 3);
        let result = tool
            .execute(json!({"agent": "researcher", "prompt": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("depth limit"));
    }

    #[tokio::test]
    async fn depth_limit_per_agent() {
        // coder has max_depth=2, so depth=2 should be blocked
        let tool = DelegateTool::with_depth(sample_agents(), None, test_security(), 2);
        let result = tool.execute(json!({"agent": "coder", "prompt": "test"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("depth limit"));
    }

    #[test]
    fn empty_agents_schema() {
        let tool = DelegateTool::new(HashMap::new(), None, test_security());
        let schema = tool.parameters_schema();
        let desc = schema["properties"]["agent"]["description"].as_str().unwrap();
        assert!(desc.contains("none configured"));
    }

    #[tokio::test]
    async fn invalid_provider_returns_error() {
        let mut agents = HashMap::new();
        agents.insert(
            "broken".to_string(),
            DelegateAgentConfig {
                provider: "totally-invalid-provider".to_string(),
                model: "model".to_string(),
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
        let tool = DelegateTool::new(agents, None, test_security());
        let result = tool
            .execute(json!({"agent": "broken", "prompt": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Failed to create provider"));
    }

    #[tokio::test]
    async fn blank_agent_rejected() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        let result = tool.execute(json!({"agent": "  ", "prompt": "test"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn blank_prompt_rejected() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        let result = tool
            .execute(json!({"agent": "researcher", "prompt": "  \t  "}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn whitespace_agent_name_trimmed_and_found() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        // " researcher " with surrounding whitespace — after trim becomes "researcher"
        let result = tool
            .execute(json!({"agent": " researcher ", "prompt": "test"}))
            .await
            .unwrap();
        // Should find "researcher" after trim — will fail at provider level
        // since ollama isn't running, but must NOT get "Unknown agent".
        assert!(result.error.is_none() || !result.error.as_deref().unwrap_or("").contains("Unknown agent"));
    }

    #[tokio::test]
    async fn delegation_blocked_in_readonly_mode() {
        let readonly = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = DelegateTool::new(sample_agents(), None, readonly);
        let result = tool
            .execute(json!({"agent": "researcher", "prompt": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only mode"));
    }

    #[tokio::test]
    async fn delegation_blocked_when_rate_limited() {
        let limited = Arc::new(SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = DelegateTool::new(sample_agents(), None, limited);
        let result = tool
            .execute(json!({"agent": "researcher", "prompt": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Rate limit exceeded"));
    }

    #[tokio::test]
    async fn delegate_context_is_prepended_to_prompt() {
        let mut agents = HashMap::new();
        agents.insert(
            "tester".to_string(),
            DelegateAgentConfig {
                provider: "invalid-for-test".to_string(),
                model: "test-model".to_string(),
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
        let tool = DelegateTool::new(agents, None, test_security());
        let result = tool
            .execute(json!({
                "agent": "tester",
                "prompt": "do something",
                "context": "some context data"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Failed to create provider")
        );
    }

    #[tokio::test]
    async fn delegate_records_request_and_result_message_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let mut agents = HashMap::new();
        agents.insert(
            "tester".to_string(),
            DelegateAgentConfig {
                provider: "mock".to_string(),
                model: "test-model".to_string(),
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
        let security = Arc::new(SecurityPolicy {
            workspace_dir: tmp.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let tool = DelegateTool::new(agents, None, security).with_shared_memory(memory.clone());

        let result = tool
            .execute(json!({
                "agent": "tester",
                "prompt": "delegate through fabric",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "sender": "alice",
                    "channel": "telegram",
                    "chat_type": "direct",
                    "chat_id": "chat-1",
                    "topic_id": "topic-1",
                    "task_id": "parent-task",
                    "message_event_id": "msg-1"
                }
            }))
            .await
            .unwrap();
        assert!(result.success);

        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: Some("tester".to_string()),
                    persona_id: None,
                    session_key: Some("delegate:telegram:chat-1:alice".to_string()),
                    channel: Some("telegram".to_string()),
                    sender: Some("alice".to_string()),
                    owner_id: None,
                },
                0,
                10,
            )
            .await
            .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].source, "delegate");
        assert_eq!(events[0].role, "user");
        assert_eq!(events[0].content, "delegate through fabric");
        assert_eq!(events[1].source, "delegate");
        assert_eq!(events[1].role, "event");
        assert!(events[1].content.contains("tester"));

        let task_events = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: Some("tester".to_string()),
                    persona_id: None,
                    session_key: Some("delegate:telegram:chat-1:alice".to_string()),
                    channel: Some("telegram".to_string()),
                    sender: Some("alice".to_string()),
                    owner_id: None,
                },
                0,
                10,
            )
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.subject_table == "tasks")
            .collect::<Vec<_>>();
        let event_types = task_events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"delegate.task.started"));
        assert!(event_types.contains(&"delegate.task.completed"));
        assert!(task_events.iter().all(|event| !event.subject_id.is_empty()));
        assert!(task_events.iter().any(|event| {
            event
                .payload_json
                .as_deref()
                .is_some_and(|payload| payload.contains("\"source_message_event_id\":\"msg-1\""))
        }));
    }

    #[test]
    fn delegate_event_scope_is_derived_from_runtime_envelope() {
        let scope = DelegateScope {
            sender: "alice".to_string(),
            channel: "telegram".to_string(),
            chat_type: "direct".to_string(),
            chat_id: "chat-1".to_string(),
            owner_id: Some("owner:/tmp/ws:telegram:alice".to_string()),
            topic_id: Some("topic-a".to_string()),
            task_id: Some("task-a".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
        };
        let event_scope = delegate_event_scope("/tmp/ws", "run-delegate", "tester", Some(&scope));

        assert_eq!(event_scope.source, "delegate");
        assert_eq!(event_scope.channel.as_deref(), Some("telegram"));
        assert_eq!(
            event_scope.session_key.as_deref(),
            Some("delegate:telegram:chat-1:alice")
        );
        assert_eq!(event_scope.run_id.as_deref(), Some("run-delegate"));
        assert_eq!(event_scope.agent_id.as_deref(), Some("tester"));
        assert_eq!(event_scope.sender.as_deref(), Some("alice"));
        assert_eq!(event_scope.recipient.as_deref(), Some("chat-1"));
        assert_eq!(event_scope.owner_id.as_deref(), Some("owner:/tmp/ws:telegram:alice"));
    }

    #[test]
    fn parse_delegate_scope_preserves_owner_topic_task_lineage() {
        let scope = parse_delegate_scope(&json!({
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "sender": "alice",
                "channel": "telegram",
                "chat_type": "direct",
                "chat_id": "chat-1",
                "owner_id": "owner:/tmp/ws:telegram:alice",
                "topic_id": "topic-a",
                "task_id": "task-a",
                "source_message_event_id": "msg-a"
            }
        }))
        .expect("trusted scope should parse");

        assert_eq!(scope.owner_id.as_deref(), Some("owner:/tmp/ws:telegram:alice"));
        assert_eq!(scope.topic_id.as_deref(), Some("topic-a"));
        assert_eq!(scope.task_id.as_deref(), Some("task-a"));
        assert_eq!(scope.source_message_event_id.as_deref(), Some("msg-a"));
    }

    #[tokio::test]
    async fn delegate_empty_context_omits_prefix() {
        let mut agents = HashMap::new();
        agents.insert(
            "tester".to_string(),
            DelegateAgentConfig {
                provider: "invalid-for-test".to_string(),
                model: "test-model".to_string(),
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
        let tool = DelegateTool::new(agents, None, test_security());
        let result = tool
            .execute(json!({
                "agent": "tester",
                "prompt": "do something",
                "context": ""
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Failed to create provider")
        );
    }

    #[test]
    fn delegate_depth_construction() {
        let tool = DelegateTool::with_depth(sample_agents(), None, test_security(), 5);
        assert_eq!(tool.depth, 5);
    }

    #[tokio::test]
    async fn delegate_no_agents_configured() {
        let tool = DelegateTool::new(HashMap::new(), None, test_security());
        let result = tool.execute(json!({"agent": "any", "prompt": "test"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("none configured"));
    }

    #[tokio::test]
    async fn agentic_mode_rejects_empty_allowed_tools() {
        let mut agents = HashMap::new();
        agents.insert("agentic".to_string(), agentic_config(Vec::new(), 10));

        let tool = DelegateTool::new(agents, None, test_security());
        let result = tool
            .execute(json!({"agent": "agentic", "prompt": "test"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("allowed_tools is empty"));
    }

    #[tokio::test]
    async fn agentic_mode_rejects_unmatched_allowed_tools() {
        let mut agents = HashMap::new();
        agents.insert(
            "agentic".to_string(),
            agentic_config(vec!["missing_tool".to_string()], 10),
        );

        let tool =
            DelegateTool::new(agents, None, test_security()).with_parent_tools(Arc::new(vec![Arc::new(EchoTool)]));
        let result = tool
            .execute(json!({"agent": "agentic", "prompt": "test"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("no executable tools"));
    }

    #[tokio::test]
    async fn execute_agentic_runs_tool_call_loop_with_filtered_tools() {
        let config = agentic_config(vec!["echo_tool".to_string()], 10);
        let tool = DelegateTool::new(HashMap::new(), None, test_security()).with_parent_tools(Arc::new(vec![
            Arc::new(EchoTool),
            Arc::new(DelegateTool::new(HashMap::new(), None, test_security())),
        ]));

        let provider = OneToolThenFinalProvider;
        let result = tool
            .execute_agentic(
                "agentic",
                &config,
                "openrouter",
                "model-test",
                &provider,
                "run",
                0.2,
                None,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("(openrouter/model-test, agentic)"));
        assert!(result.output.contains("done"));
    }

    #[tokio::test]
    async fn execute_agentic_excludes_delegate_even_if_allowlisted() {
        let config = agentic_config(vec!["delegate".to_string()], 10);
        let tool =
            DelegateTool::new(HashMap::new(), None, test_security()).with_parent_tools(Arc::new(vec![Arc::new(
                DelegateTool::new(HashMap::new(), None, test_security()),
            )]));

        let provider = OneToolThenFinalProvider;
        let result = tool
            .execute_agentic(
                "agentic",
                &config,
                "openrouter",
                "model-test",
                &provider,
                "run",
                0.2,
                None,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("no executable tools"));
    }

    #[tokio::test]
    async fn execute_agentic_respects_max_iterations() {
        let config = agentic_config(vec!["echo_tool".to_string()], 2);
        let tool = DelegateTool::new(HashMap::new(), None, test_security())
            .with_parent_tools(Arc::new(vec![Arc::new(EchoTool)]));

        let provider = InfiniteToolCallProvider;
        let result = tool
            .execute_agentic(
                "agentic",
                &config,
                "openrouter",
                "model-test",
                &provider,
                "run",
                0.2,
                None,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("maximum tool iterations (2)")
        );
    }

    #[tokio::test]
    async fn execute_agentic_propagates_provider_errors() {
        let config = agentic_config(vec!["echo_tool".to_string()], 10);
        let tool = DelegateTool::new(HashMap::new(), None, test_security())
            .with_parent_tools(Arc::new(vec![Arc::new(EchoTool)]));

        let provider = FailingProvider;
        let result = tool
            .execute_agentic(
                "agentic",
                &config,
                "openrouter",
                "model-test",
                &provider,
                "run",
                0.2,
                None,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("provider boom"));
    }

    fn single_agent(name: &str, provider: &str, model: &str) -> HashMap<String, DelegateAgentConfig> {
        let mut agents = HashMap::new();
        agents.insert(
            name.to_string(),
            DelegateAgentConfig {
                provider: provider.to_string(),
                model: model.to_string(),
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
        agents
    }

    #[test]
    fn schema_exposes_inline_model_and_provider() {
        let tool = DelegateTool::new(sample_agents(), None, test_security());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["model"].is_object());
        assert!(schema["properties"]["provider"].is_object());
        // Inline overrides must remain optional for backward compatibility.
        let required = schema["required"].as_array().unwrap();
        assert!(!required.contains(&json!("model")));
        assert!(!required.contains(&json!("provider")));
    }

    #[tokio::test]
    async fn delegate_inline_model_overrides_agent_config() {
        // Agent config model is "config-model"; inline override pins "pinned-model".
        let tool = DelegateTool::new(single_agent("tester", "mock", "config-model"), None, test_security());
        let result = tool
            .execute(json!({
                "agent": "tester",
                "prompt": "hello",
                "model": "pinned-model"
            }))
            .await
            .unwrap();

        assert!(result.success, "mock provider should succeed: {:?}", result.error);
        // The formatted output records the effective (overridden) model.
        assert!(
            result.output.contains("pinned-model"),
            "output should reflect inline model: {}",
            result.output
        );
        assert!(
            !result.output.contains("config-model"),
            "inline model must replace config model: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn delegate_inline_provider_overrides_agent_config() {
        // Agent config provider is valid ("mock"); inline override forces an
        // invalid provider so we can observe that the override path is taken
        // (provider creation fails naming the override, not the config value).
        let tool = DelegateTool::new(single_agent("tester", "mock", "m"), None, test_security());
        let result = tool
            .execute(json!({
                "agent": "tester",
                "prompt": "hello",
                "provider": "totally-invalid-provider"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            err.contains("totally-invalid-provider"),
            "error should name the inline provider override: {err}"
        );
    }

    #[tokio::test]
    async fn delegate_without_inline_uses_agent_config() {
        // No inline model/provider → backward-compatible: agent config wins.
        let tool = DelegateTool::new(single_agent("tester", "mock", "config-model"), None, test_security());
        let result = tool
            .execute(json!({
                "agent": "tester",
                "prompt": "hello"
            }))
            .await
            .unwrap();

        assert!(result.success, "mock provider should succeed: {:?}", result.error);
        assert!(
            result.output.contains("mock/config-model"),
            "output should reflect agent config provider/model: {}",
            result.output
        );
    }
}
