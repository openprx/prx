use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_contracts::ScopeContext;
use crate::config::DelegateAgentConfig;
use crate::hooks::HookManager;
use crate::memory::{Memory, MemoryEventRecording, MemoryFabric, MessageEventScope};
use crate::observability::traits::{Observer, ObserverEvent, ObserverMetric};
use crate::providers::{self, Provider};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::SecurityPolicy;
use crate::security::policy::ToolOperation;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{Instrument as _, instrument::WithSubscriber as _};
#[cfg(test)]
mod task_size_tests;
// Box and erase owned suspension fields before caller polls or task spawning.
#[inline(never)]
fn boxed_delegate_field<'a, F>(
    make: impl FnOnce() -> F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + Send + 'a>>
where
    F: std::future::Future + Send + 'a,
{
    Box::pin(make())
}

#[inline(never)]
fn try_boxed_delegate_field<'a, F>(
    make: impl FnOnce() -> anyhow::Result<F>,
) -> anyhow::Result<std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + Send + 'a>>>
where
    F: std::future::Future + Send + 'a,
{
    Ok(Box::pin(make()?))
}

/// The ordinary parent publication and the caller context a delegate child
/// executes under (the only child issuance path).
struct DelegateChildAuthority {
    publication: crate::runtime::child_source::publication::OrdinaryChildPublication,
    caller: crate::tools::ToolExecutionContext,
}

struct DelegateChildSource {
    authority: DelegateChildAuthority,
    proof: Arc<crate::runtime::turn_source::OwnedTurnSourceProof>,
    execution: crate::tools::ToolExecutionContext,
    fabric: MemoryFabric,
}

struct DelegateAgenticExecution {
    // True only after ChildSegmentTurn committed its terminal result.
    child_terminal_committed: bool,
    result: ToolResult,
    trace: Option<crate::agent::turn_contracts::ToolLoopTrace>,
}

/// Tool that delegates a subtask to a named agent with a different
/// provider/model configuration. Enables multi-agent workflows where
/// a primary agent can hand off specialized work (research, coding,
/// summarization) to purpose-built sub-agents.
pub struct DelegateTool {
    agents: Arc<HashMap<String, DelegateAgentConfig>>,
    security: Arc<SecurityPolicy>,
    /// The root connection a delegation falls back to (see
    /// [`RootConnection`]); empty unless [`DelegateTool::with_root_connection`]
    /// set it.
    root: RootConnection,
    /// The providers the operator configured: the only ones a per-call
    /// `provider` argument may name.
    configured_providers: providers::ConfiguredProviders,
    /// What the configured models can do: operator declarations and cached
    /// catalog metadata. Empty means the adapter bounds only.
    model_catalog: Arc<providers::catalog::CatalogView>,
    /// Pinned configuration for resolving an explicit model hint and its
    /// owner-bound credential. Absent only in isolated legacy tool fixtures.
    hint_config: Option<Arc<crate::config::Config>>,
    /// Reliability settings for the per-agent provider chain.
    reliability: crate::config::ReliabilityConfig,
    /// Depth at which this tool instance lives in the delegation chain.
    depth: u32,
    /// Parent tool registry for agentic sub-agents.
    parent_tools: Arc<Vec<Arc<dyn Tool>>>,
    parent_hooks: Option<Arc<HookManager>>,
    /// Inherited multimodal handling config for sub-agent loops.
    multimodal_config: crate::config::MultimodalConfig,
    /// Shared memory fabric backend for normalized delegation events.
    memory: Option<Arc<dyn Memory>>,
    event_recording: MemoryEventRecording,
}

#[derive(Debug, Clone)]
struct DelegateScope {
    sender: String,
    channel: String,
    chat_id: String,
    owner_id: Option<String>,
    topic_id: Option<String>,
    task_id: Option<String>,
    source_message_event_id: Option<String>,
    config_generation_id: Option<u64>,
    config_source_revision: Option<String>,
}

impl DelegateScope {
    fn from_execution_context(context: &crate::tools::ToolExecutionContext) -> Self {
        Self {
            sender: context.sender().to_owned(),
            channel: context.channel().to_owned(),
            chat_id: context.chat_id.clone(),
            owner_id: context.envelope.owner_id.clone(),
            topic_id: context.envelope.topic_id.clone(),
            task_id: context.envelope.task_id.clone(),
            source_message_event_id: context.envelope.source_message_event_id.clone(),
            config_generation_id: context.envelope.config_generation_id,
            config_source_revision: context.envelope.config_source_revision.clone(),
        }
    }
}

// Only a typed unsupported-provider refusal is an operational tool failure.
// Credential, route, generation and authority errors keep their original Result.
fn delegate_provider_failure(error: anyhow::Error) -> anyhow::Result<ToolResult> {
    match error.downcast_ref::<providers::catalog::ResolutionError>() {
        Some(refusal @ providers::catalog::ResolutionError::UnknownProvider { .. }) => Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(refusal.to_string()),
        }),
        _ => Err(error),
    }
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
    let _chat_type = scope
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
        config_generation_id: scope.get("config_generation_id").and_then(serde_json::Value::as_u64),
        config_source_revision: scope
            .get("config_source_revision")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

/// The root connection: the provider the root credential and base URL were
/// configured for, with that credential and URL. The three travel as one
/// unit (INT-2B a32): the credential and the URL are released only to a
/// child on that same provider identity, never one without the other.
#[derive(Clone, Default)]
pub struct RootConnection {
    /// The provider the root values were configured for
    /// (`default_provider`). Without it nothing is released.
    pub provider: Option<String>,
    /// The root credential (`api_key`), if any.
    pub key: Option<String>,
    /// The root base URL (`api_url`), if any.
    pub api_url: Option<String>,
}

impl std::fmt::Debug for RootConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootConnection")
            .field(
                "provider",
                &self.provider.as_deref().map(providers::redact_endpoint_userinfo),
            )
            .field("key", &self.key.as_ref().map(|_| "[REDACTED]"))
            .field(
                "api_url",
                &self.api_url.as_deref().map(providers::redact_endpoint_userinfo),
            )
            .finish()
    }
}

impl DelegateTool {
    pub fn new(agents: HashMap<String, DelegateAgentConfig>, security: Arc<SecurityPolicy>) -> Self {
        let configured_providers = providers::ConfiguredProviders::default()
            .with_providers(agents.values().map(|agent| agent.provider.as_str()));
        Self {
            agents: Arc::new(agents),
            security,
            root: RootConnection::default(),
            configured_providers,
            model_catalog: Arc::default(),
            hint_config: None,
            reliability: crate::config::ReliabilityConfig::default(),
            depth: 0,
            parent_tools: Arc::new(Vec::new()),
            parent_hooks: None,
            multimodal_config: crate::config::MultimodalConfig::default(),
            memory: None,
            event_recording: MemoryEventRecording::default(),
        }
    }

    /// Supply the deployment's reliability settings for the per-agent chain.
    #[must_use]
    pub fn with_reliability(mut self, reliability: crate::config::ReliabilityConfig) -> Self {
        self.reliability = reliability;
        self
    }

    /// Create a DelegateTool for a sub-agent (with incremented depth).
    /// When sub-agents eventually get their own tool registry, construct
    /// their DelegateTool via this method with `depth: parent.depth + 1`.
    pub fn with_depth(agents: HashMap<String, DelegateAgentConfig>, security: Arc<SecurityPolicy>, depth: u32) -> Self {
        let configured_providers = providers::ConfiguredProviders::default()
            .with_providers(agents.values().map(|agent| agent.provider.as_str()));
        Self {
            agents: Arc::new(agents),
            security,
            root: RootConnection::default(),
            configured_providers,
            model_catalog: Arc::default(),
            hint_config: None,
            reliability: crate::config::ReliabilityConfig::default(),
            depth,
            parent_tools: Arc::new(Vec::new()),
            parent_hooks: None,
            multimodal_config: crate::config::MultimodalConfig::default(),
            memory: None,
            event_recording: MemoryEventRecording::default(),
        }
    }

    /// Share the owning runtime manager independently of exposed control tools.
    pub fn with_parent_hooks(mut self, hooks: Arc<HookManager>) -> Self {
        self.parent_hooks = Some(hooks);
        self
    }

    /// Attach parent tools used to build sub-agent allowlist registries.
    pub fn with_parent_tools(mut self, parent_tools: Arc<Vec<Arc<dyn Tool>>>) -> Self {
        self.parent_tools = parent_tools;
        self
    }

    /// Set the root connection a delegation may fall back to (the root
    /// `default_provider`, `api_key` and `api_url` together). The provider is
    /// also made selectable by a per-call `provider` argument.
    #[must_use]
    pub fn with_root_connection(mut self, root: RootConnection) -> Self {
        if let Some(name) = root.provider.as_deref() {
            self.configured_providers = self.configured_providers.with_providers(std::iter::once(name));
        }
        self.root = root;
        self
    }

    /// Widen the providers a per-call `provider` argument may name to every
    /// provider the operator configured (see
    /// [`providers::ConfiguredProviders::from_config`]). Without it only the
    /// named agents' providers and the fallback provider are selectable.
    #[must_use]
    pub fn with_configured_providers(mut self, configured: &providers::ConfiguredProviders) -> Self {
        self.configured_providers = self.configured_providers.union(configured);
        self
    }

    /// Supply the model evidence a delegation is resolved against
    /// (`providers::selection::catalog_for`).
    #[must_use]
    pub fn with_model_catalog(mut self, catalog: providers::catalog::CatalogView) -> Self {
        self.model_catalog = Arc::new(catalog);
        self
    }

    #[must_use]
    pub fn with_hint_config(mut self, config: &crate::config::Config) -> Self {
        self.hint_config = Some(Arc::new(config.clone()));
        self
    }

    /// The connection `effective_provider` is built with.
    ///
    /// - The named agent's own key, for the agent's own provider identity. The
    ///   agent config has no base URL, so none is attached.
    /// - Otherwise the root connection, for the provider identity it was
    ///   configured for: its key (if any) *together with* the base URL it was
    ///   configured with, also when the root has no key. `released` is set,
    ///   so the root rotation keys may follow.
    /// - Otherwise nothing: the provider resolves its own auth profile or runs
    ///   keyless.
    ///
    /// A credential is never detached from the endpoint it belongs to.
    #[cfg(test)]
    fn child_connection<'a>(
        &'a self,
        agent_config: &'a DelegateAgentConfig,
        effective_provider: &str,
    ) -> providers::ProviderConnection<'a> {
        if let Some(agent_key) = providers::credential_for_provider_identity(
            effective_provider,
            &[(Some(agent_config.provider.as_str()), agent_config.api_key.as_deref())],
        ) {
            return providers::ProviderConnection {
                api_key: Some(agent_key),
                api_url: None,
                released: false,
            };
        }
        let Some(owner) = self.root.provider.as_deref() else {
            return providers::ProviderConnection {
                api_key: None,
                api_url: None,
                released: false,
            };
        };
        // Released to the same provider identity with or without a key: a
        // keyless root (a remote Ollama given by `api_url`) keeps its base
        // URL, as the worker path does (INT-2B a33, D-030). Another provider
        // gets nothing.
        providers::configured_connection_for(
            effective_provider,
            owner,
            self.root.key.as_deref().map(str::trim).filter(|key| !key.is_empty()),
            self.root.api_url.as_deref(),
        )
    }

    /// Attach multimodal configuration for sub-agent tool loops.
    pub const fn with_multimodal_config(mut self, config: crate::config::MultimodalConfig) -> Self {
        self.multimodal_config = config;
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
        .with_agent_id(agent_name)
        // A delegate's logical task is its own run. The caller's task is lineage
        // (`parent_task_id` in the task payloads), never child authority.
        .with_task_id(delegate_run_id);
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
        if let Some(source_message_event_id) = &scope.source_message_event_id {
            envelope = envelope.with_source_message_event_id(source_message_event_id.clone());
        }
        envelope.config_generation_id = scope.config_generation_id;
        envelope.config_source_revision = scope.config_source_revision.clone();
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
        // Sorted: the contract is fingerprinted, and a worker process rebuilds
        // this tool from the same agents in its own hash order.
        let mut agent_names: Vec<&str> = self.agents.keys().map(|s: &String| s.as_str()).collect();
        agent_names.sort_unstable();
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
                    "description": "Optional provider override. Must be a provider the operator configured (a named agent's provider, the default provider, a fallback provider or a model route); any other provider, including an unconfigured 'custom:<url>', is refused. Defaults to the named agent's configured provider."
                }
            },
            "required": ["agent", "prompt"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        #[cfg(all(test, unix))]
        delegate_diagnostic::phase("delegate-body.json");
        let caller = crate::tools::execution::current_child_call_context();
        if let Some(caller) = caller.as_ref() {
            caller.verify_tool("delegate")?;
        }
        let scope = caller
            .as_ref()
            .map(|caller| DelegateScope::from_execution_context(caller.context()))
            .or_else(|| parse_delegate_scope(&args));
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

        // Select the actual admitted parent generation before agent, provider
        // override and credential resolution. All execution settings belong
        // to this generation, including disabled-plane child turns.
        let ordinary_publication = caller
            .as_ref()
            .and_then(|caller| caller.context().ordinary_child_publication.as_ref());
        if let (Some(publication), Some(caller)) = (ordinary_publication, caller.as_ref()) {
            publication.verify_caller(caller.context())?;
            publication.parent_authority().ensure_child_available()?;
        }
        let Some(ordinary_publication) = ordinary_publication else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("delegation requires an admitted parent source".into()),
            });
        };
        let security = ordinary_publication.security();
        let child_config = ordinary_publication.generation().effective.as_ref();
        let agents = &child_config.agents;
        let agent_config = match agents.get(agent_name) {
            Some(cfg) => cfg,
            None => {
                let available: Vec<&str> = agents.keys().map(|s: &String| s.as_str()).collect();
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

        if let Err(error) = security.enforce_tool_operation(ToolOperation::Act, "delegate") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        // A per-call provider is accepted only among the providers the operator
        // configured: a prompt must not be able to direct this request, or a
        // credential, to a host the operator never chose.
        if let Some(requested) = provider_override.as_deref() {
            let configured = providers::ConfiguredProviders::from_config(child_config);
            if let Err(error) = configured.ensure_permits(requested) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(error.to_string()),
                });
            }
        }

        // Resolve effective provider/model: inline override > agent config.
        // When omitted, fall back to the agent config (backward compatible).
        let mut effective_provider = provider_override
            .clone()
            .unwrap_or_else(|| agent_config.provider.clone());
        let mut effective_model = model_override.clone().unwrap_or_else(|| agent_config.model.clone());

        let child_binding_result = {
            let config = child_config;

            let owns_provider =
                providers::ProviderId::of(&effective_provider) == providers::ProviderId::of(&agent_config.provider);
            let credential = if owns_provider {
                agent_config.credential_ref.as_deref().map_or_else(
                    || {
                        if agent_config
                            .api_key
                            .as_deref()
                            .is_some_and(|key| !key.trim().is_empty())
                        {
                            providers::credentials::CredentialRef::LegacyInline(
                                providers::credentials::LegacySlot::Agent(agent_name.to_owned()),
                            )
                        } else {
                            providers::selection::credential_for(config, &effective_provider)
                        }
                    },
                    |id| providers::credentials::CredentialRef::Profile { id: id.to_owned() },
                )
            } else {
                providers::selection::credential_for(config, &effective_provider)
            };
            crate::runtime::child_source::ChildProviderBinding::build_ordinary(
                config,
                &self.model_catalog,
                &effective_provider,
                &effective_model,
                credential,
                Some(ordinary_publication),
            )
        };
        let child_binding = match child_binding_result {
            Ok(binding) => binding,
            Err(error) => return delegate_provider_failure(error),
        };
        effective_provider = child_binding.provider_name().to_owned();
        effective_model = child_binding.model().to_owned();
        let provider = child_binding.provider();
        // Every text this tool writes for the parent model or into a durable
        // event names the provider this way: a `custom:` URL may carry
        // `user:password@`. The configured label stays the connection key.
        let display_provider = providers::redact_endpoint_userinfo(&effective_provider).into_owned();
        // Build the message
        let full_prompt = if context.is_empty() {
            prompt.to_string()
        } else {
            format!("[Context]\n{context}\n\n[Task]\n{prompt}")
        };

        let temperature = agent_config.temperature.unwrap_or(0.7);
        let delegate_run_id = uuid::Uuid::new_v4().to_string();
        let memory_fabric = ordinary_publication.source_fabric();
        let mut event_scope = delegate_event_scope(
            &security.workspace_dir.to_string_lossy(),
            &delegate_run_id,
            agent_name,
            scope.as_ref(),
        );
        child_binding.bind_ordinary_input_scope(
            &provider,
            &security.workspace_dir.to_string_lossy(),
            &mut event_scope,
            caller.as_ref(),
        )?;
        // Box the measured owned input-recording future at its construction boundary.
        // The parent retains cancellation ownership and awaits it in the same position.
        let source_receipt = boxed_delegate_field(|| {
            crate::runtime::child_source::record_child_input(
                &memory_fabric,
                event_scope.clone(),
                &full_prompt,
                Some(format!("delegate:{delegate_run_id}:request")),
                Some(
                    json!({
                        "agent": agent_name,
                        "provider": display_provider,
                        "model": effective_model,
                        "agentic": agent_config.agentic,
                        "has_context": !context.is_empty()
                    })
                    .to_string(),
                ),
            )
        })
        .await?;
        record_delegate_task_event(
            &memory_fabric,
            event_scope.clone(),
            "delegate.task.started",
            json!({
                "agent": agent_name,
                "provider": display_provider,
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
        let child_source = if let Some(caller) = caller.as_ref()
            && let Some(publication) = &caller.context().ordinary_child_publication
        {
            // Full parents keep the named child Full; below Full the
            // configured agent level is sealed into its scope.
            let level = crate::tools::capability::child::ChildLevel::from_named(
                crate::security::agent_level::named_child_level(
                    &publication.generation().effective,
                    Some(agent_name),
                    publication.level(),
                ),
            );
            let publication = publication.with_child_level(level)?;
            let caller = caller.with_ordinary_publication(publication.clone())?;
            let (proof, execution) = child_binding.ordinary_execution_source(
                &provider,
                &caller,
                &event_scope,
                &full_prompt,
                source_receipt.as_deref(),
            )?;
            Some(DelegateChildSource {
                authority: DelegateChildAuthority {
                    publication,
                    caller: caller.context().clone(),
                },
                proof,
                execution,
                fabric: memory_fabric.clone(),
            })
        } else {
            None
        };
        let child_source =
            child_source.ok_or_else(|| anyhow::anyhow!("delegate requires its admitted child source"))?;
        // Every model call belongs to the admitted shared child turn, including
        // non-agentic children. Their G7 constraints forbid tool execution.
        let execution = self
            .spawn_delegate_loop(DelegateTaskRequest {
                agent_name: agent_name.to_owned(),
                effective_provider: effective_provider.clone(),
                effective_model: effective_model.clone(),
                provider,
                full_prompt,
                temperature,
                child_source,
            })
            .await?;
        anyhow::ensure!(
            execution.child_terminal_committed || (!execution.result.success && execution.trace.is_none()),
            "delegate model execution ended without its child terminal"
        );
        // A validation failure before child-turn issuance has no model outcome
        // to settle. Preserve its task failure events without fabricating one.
        let tool_result = execution.result;
        {
            let result = Ok(tool_result.clone());
            boxed_delegate_field(|| record_delegate_result_event(&memory_fabric, event_scope.clone(), &result)).await;
            record_delegate_terminal_task_event(
                &memory_fabric,
                event_scope,
                agent_name,
                &display_provider,
                &effective_model,
                agent_config.agentic,
                scope.as_ref(),
                &result,
            )
            .await;
        }
        Ok(tool_result)
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
    // The result record carries the delegate's reply or error text: redacted
    // like the terminal commit (D-053).
    if let Err(error) = fabric
        .record_worker_result(
            scope,
            crate::agent::terminal::redact_persisted_text(&content),
            Some(
                crate::agent::terminal::redact_persisted_json(json!({ "success": success, "error": error }))
                    .to_string(),
            ),
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
    // Task payloads carry the delegate's error and an output preview: redacted
    // like the terminal commit (D-053).
    let payload = crate::agent::terminal::redact_persisted_json(payload);
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
) {
    let (success, error, output_preview) = match result {
        Ok(result) => (
            result.success,
            result.error.clone(),
            result.output.chars().take(500).collect::<String>(),
        ),
        Err(error) => (false, Some(error.to_string()), String::new()),
    };
    let event_type = if success {
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

// Only shared immutable resource handles cross the task boundary. All turn
// state, admission, cancellation and cleanup ownership move into the task.
struct DelegateTaskRequest {
    agent_name: String,
    effective_provider: String,
    effective_model: String,
    provider: Arc<dyn Provider>,
    full_prompt: String,
    temperature: f64,
    child_source: DelegateChildSource,
}

struct DelegateLoopRequest<'a> {
    agent_name: &'a str,
    agent_config: &'a DelegateAgentConfig,
    effective_provider: &'a str,
    effective_model: &'a str,
    provider: &'a dyn Provider,
    full_prompt: &'a str,
    temperature: f64,
    scope: Option<&'a DelegateScope>,
    child_source: Option<DelegateChildSource>,
    task_context: Option<DelegateTaskContext>,
}

struct DelegateLoopResources {
    security: Arc<SecurityPolicy>,
    parent_tools: Arc<Vec<Arc<dyn Tool>>>,
    parent_hooks: Option<Arc<HookManager>>,
}

struct DelegateTaskContext {
    cancellation: tokio_util::sync::CancellationToken,
    cleanup_owner: Option<Arc<crate::runtime::turn_cleanup::CleanupTracker>>,
    // Drop the cleanup owner before its acknowledgement receiver, as before.
    loop_cleanup: crate::runtime::turn_cleanup::CleanupScope,
    cleanup_confirmed: tokio::sync::oneshot::Receiver<Result<(), String>>,
}

fn resolve_pinned_child_compaction(
    config: &crate::config::Config,
    provider: &str,
    model: &str,
) -> crate::providers::context_window::EffectiveCompactionConfig {
    let catalog = crate::providers::selection::catalog_for(config);
    crate::providers::context_window::resolve_effective_compaction_config_with_catalog(
        &config.agent.compaction,
        provider,
        model,
        crate::providers::context_window::ContextWindowSources::from_config(config),
        &config.model_routes,
        &catalog,
    )
}

impl DelegateTaskContext {
    fn new() -> anyhow::Result<Self> {
        let cleanup_owner = crate::runtime::turn_cleanup::current_tracker();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let (cleanup_ack, cleanup_confirmed) = tokio::sync::oneshot::channel();
        let loop_cleanup =
            crate::runtime::turn_cleanup::CleanupScope::new(cleanup_owner.as_ref(), cancellation.clone())?
                .with_acknowledgement(Some(cleanup_ack));
        Ok(Self {
            cancellation,
            cleanup_owner,
            loop_cleanup,
            cleanup_confirmed,
        })
    }
}

struct DelegateTask(tokio::task::JoinHandle<anyhow::Result<DelegateAgenticExecution>>);

impl Drop for DelegateTask {
    fn drop(&mut self) {
        // Abort is a request, not cleanup confirmation. The moved CleanupScope
        // retains the parent's ticket until its existing owned drain finishes.
        self.0.abort();
    }
}

impl DelegateTool {
    fn loop_resources(&self) -> DelegateLoopResources {
        DelegateLoopResources {
            security: Arc::clone(&self.security),
            parent_tools: Arc::clone(&self.parent_tools),
            parent_hooks: self.parent_hooks.clone(),
        }
    }

    async fn spawn_delegate_loop(&self, request: DelegateTaskRequest) -> anyhow::Result<DelegateAgenticExecution> {
        let DelegateTaskRequest {
            agent_name,
            effective_provider,
            effective_model,
            provider,
            full_prompt,
            temperature,
            child_source,
        } = request;
        let resources = self.loop_resources();
        let config = Arc::clone(&child_source.authority.publication.generation().effective);
        // Register before spawn: even cancellation before the first poll keeps
        // the child represented in the parent's cleanup tracker.
        let task_context = DelegateTaskContext::new()?;
        let owner = Arc::clone(
            task_context
                .cleanup_owner
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("delegate child cleanup owner missing"))?,
        );
        // Carry lineage and restriction metadata, never the parent's mutable
        // terminal collector or incoming/outgoing execution authority.
        let spawn_metadata = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
            .try_with(|parent| {
                let mut metadata = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
                    parent.run_id.clone(),
                    parent.session_scope_key.clone(),
                )
                .with_channel_excluded_tools(parent.channel_excluded_tools.clone());
                metadata.task_input = parent.task_input.clone();
                metadata.spawn_depth = parent.spawn_depth;
                metadata.is_turn_root = parent.is_turn_root;
                metadata.owner_id = parent.owner_id.clone();
                metadata.topic_id = parent.topic_id.clone();
                metadata.source_message_event_id = parent.source_message_event_id.clone();
                metadata
            })
            .ok();
        let parent_work = crate::runtime::registry::current_work_id();
        let progress = crate::agent::idle::current_beat();
        let span = tracing::debug_span!("delegate_child_segment");
        let work = async move {
            owner
                .scope(async move {
                    let agent_config = config
                        .agents
                        .get(&agent_name)
                        .ok_or_else(|| anyhow::anyhow!("delegate admitted agent configuration missing"))?;
                    crate::providers::model_settings::with_temperature(
                        agent_config.temperature,
                        resources.boxed_delegate_loop(DelegateLoopRequest {
                            agent_name: &agent_name,
                            agent_config,
                            effective_provider: &effective_provider,
                            effective_model: &effective_model,
                            provider: provider.as_ref(),
                            full_prompt: &full_prompt,
                            temperature,
                            scope: None,
                            child_source: Some(child_source),
                            task_context: Some(task_context),
                        }),
                    )
                    .await
                })
                .await
        }
        .instrument(span);
        // Keep ambient scope wrappers pointer-sized while polling the child loop.
        let work = Box::pin(async move {
            let work = async move {
                match spawn_metadata {
                    Some(metadata) => {
                        crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                            .scope(metadata, work)
                            .await
                    }
                    None => work.await,
                }
            };
            let work = crate::agent::idle::scope_beat(progress, work);
            match parent_work {
                Some(id) => crate::runtime::registry::scope_current(id, work).await,
                None => work.await,
            }
        });
        #[cfg(test)]
        let work = crate::runtime::logical_clock::inherit(work);
        #[cfg(all(test, unix))]
        let work = delegate_diagnostic::inherit(work);
        let mut task = DelegateTask(crate::agent::channel_approval::spawn(boxed_delegate_field(|| {
            work.with_current_subscriber()
        })));
        match (&mut task.0).await {
            Ok(result) => result,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => Err(anyhow::Error::new(error).context(crate::runtime::turn_cleanup::CleanupIncomplete)),
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn boxed_delegate_loop(
        &self,
        agent_name: &str,
        agent_config: &DelegateAgentConfig,
        effective_provider: &str,
        effective_model: &str,
        provider: &dyn Provider,
        full_prompt: &str,
        temperature: f64,
        scope: Option<&DelegateScope>,
        child_source: Option<DelegateChildSource>,
    ) -> anyhow::Result<DelegateAgenticExecution> {
        self.loop_resources()
            .boxed_delegate_loop(DelegateLoopRequest {
                agent_name,
                agent_config,
                effective_provider,
                effective_model,
                provider,
                full_prompt,
                temperature,
                scope,
                child_source,
                task_context: None,
            })
            .await
    }
}

impl DelegateLoopResources {
    #[inline(never)]
    fn boxed_delegate_loop<'a>(
        &'a self,
        request: DelegateLoopRequest<'a>,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<DelegateAgenticExecution>> + 'a>> {
        let DelegateLoopRequest {
            agent_name,
            agent_config,
            effective_provider,
            effective_model,
            provider,
            full_prompt,
            temperature,
            scope,
            child_source,
            task_context,
        } = request;
        // Return the owned child state before its first poll. This changes no
        // authority, cancellation, cleanup or terminal-settlement boundary.
        let make = move || {
            self.execute_tool_loop(
                agent_name,
                agent_config,
                effective_provider,
                effective_model,
                provider,
                full_prompt,
                temperature,
                scope,
                child_source,
                task_context,
            )
        };
        #[cfg(all(test, unix))]
        delegate_diagnostic::future_layout("child-future-layout.json", &make);
        let work = Box::pin(make());
        #[cfg(all(test, unix))]
        delegate_diagnostic::phase("child-constructor-returned.json");
        work
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_tool_loop(
        &self,
        agent_name: &str,
        agent_config: &DelegateAgentConfig,
        effective_provider: &str,
        effective_model: &str,
        provider: &dyn Provider,
        full_prompt: &str,
        temperature: f64,
        _scope: Option<&DelegateScope>,
        child_source: Option<DelegateChildSource>,
        task_context: Option<DelegateTaskContext>,
    ) -> anyhow::Result<DelegateAgenticExecution> {
        #[cfg(all(test, unix))]
        delegate_diagnostic::phase("child-body-entered.json");
        let security = child_source.as_ref().map_or_else(
            || Arc::clone(&self.security),
            |source| Arc::clone(source.authority.publication.security()),
        );
        if let Some(source) = child_source.as_ref() {
            use crate::runtime::turn_source::TurnSourceProof;
            anyhow::ensure!(
                source.proof.routing_input() == full_prompt,
                "delegate execution input differs from its admitted source"
            );
        }
        let answer_only = child_source.is_some() && !agent_config.agentic;
        // At Full the child inherits every tool its level allows. Explicit
        // `allowed_tools` names remain direct-publication preferences.
        let full = child_source
            .as_ref()
            .is_some_and(|source| source.authority.publication.level() == crate::security::AutonomyLevel::Full);
        let requested = agent_config
            .allowed_tools
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .collect::<std::collections::HashSet<_>>();
        // An empty `allowed_tools` means all tools, the same as `["*"]`.
        let inherit_all = answer_only || full || requested.is_empty() || requested.contains("*");
        if !answer_only && !full && requested.contains("*") && requested.len() != 1 {
            return Ok(DelegateAgenticExecution {
                child_terminal_committed: false,
                result: ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Agent '{agent_name}' must use allowed_tools=['*'] exclusively")),
                },
                trace: None,
            });
        }

        let eligible_parent_tools = self
            .parent_tools
            .iter()
            .filter(|tool| child_source.is_some() || tool.name() != "delegate")
            .collect::<Vec<_>>();
        if !inherit_all {
            let mut unknown = requested
                .iter()
                .copied()
                .filter(|name| !eligible_parent_tools.iter().any(|tool| tool.supports_name(name)))
                .collect::<Vec<_>>();
            unknown.sort_unstable();
            if !unknown.is_empty() {
                return Ok(DelegateAgenticExecution {
                    child_terminal_committed: false,
                    result: ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Agent '{agent_name}' requested unknown or ineligible tools: {}",
                            unknown.join(", ")
                        )),
                    },
                    trace: None,
                });
            }
        }

        let mut sub_tools: Vec<Box<dyn Tool>> = if inherit_all || child_source.is_some() {
            // Child execution always uses exact per-call bindings and the
            // verified manifest. Keeping real roots is necessary for MCP
            // aliases and synthetic target binding/parity checks; it does not
            // add any name to that manifest or the model's selected surface.
            eligible_parent_tools
                .iter()
                .map(|tool| Box::new(ToolArcRef::new(Arc::clone(tool))) as Box<dyn Tool>)
                .collect()
        } else {
            agent_config
                .allowed_tools
                .iter()
                .map(|name| name.trim())
                .filter(|name| !name.is_empty())
                .filter_map(|name| {
                    eligible_parent_tools
                        .iter()
                        .find(|tool| tool.supports_name(name))
                        .and_then(|tool| ToolArcRef::for_name(Arc::clone(tool), name))
                })
                .map(|tool| Box::new(tool) as Box<dyn Tool>)
                .collect()
        };

        // Answer-only sources keep inherited registry parity, but may not
        // acquire an additional runtime tool dependency.
        let history_dependency_allowed = !answer_only
            && child_source.as_ref().is_none_or(|source| {
                source
                    .authority
                    .publication
                    .allows_tool(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME)
            });
        if history_dependency_allowed
            && !sub_tools
                .iter()
                .any(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
            && let Some(tool) = eligible_parent_tools
                .iter()
                .find(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
                .and_then(|tool| {
                    ToolArcRef::for_name(Arc::clone(tool), crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME)
                })
        {
            sub_tools.push(Box::new(tool));
        }

        if !answer_only && sub_tools.is_empty() {
            return Ok(DelegateAgenticExecution {
                child_terminal_committed: false,
                result: ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Agent '{agent_name}' has no executable tools after filtering allowed_tools ({})",
                        agent_config.allowed_tools.join(", ")
                    )),
                },
                trace: None,
            });
        }

        let (hooks, observer): (Arc<HookManager>, Arc<dyn Observer>) = match child_source.as_ref() {
            Some(source) => {
                let runtime = source.authority.publication.loop_runtime()?;
                (Arc::clone(&runtime.hooks), Arc::clone(&runtime.observer))
            }
            None => (
                super::child_hooks::child_hooks(self.parent_hooks.clone(), &security.workspace_dir),
                Arc::new(NoopObserver),
            ),
        };
        let child_source =
            child_source.ok_or_else(|| anyhow::anyhow!("delegate requires its admitted child source"))?;
        let child_execution = child_source.execution.clone();
        let child_event_fabric = child_source.fabric.clone();
        let child_memory = child_event_fabric.memory();
        let envelope = &child_execution.envelope;
        let scope_ctx = ScopeContext::from_execution_context(&security, &child_execution, &envelope.session_key);
        let child_config = &child_source.authority.publication.generation().effective;
        let resolved_compaction = resolve_pinned_child_compaction(child_config, effective_provider, effective_model);
        tracing::debug!(
            agent = agent_name,
            provider = %resolved_compaction.shown_provider(),
            model = resolved_compaction.selected_model.as_str(),
            max_context_tokens = resolved_compaction.config.max_context_tokens,
            source = ?resolved_compaction.max_context_source,
            kernel_capped = resolved_compaction.kernel_capped,
            "delegate resolved child compaction window"
        );

        // Execute against the same live registry that issued the sealed catalog.
        let registry = child_source.authority.publication.registry();
        let DelegateTaskContext {
            cancellation,
            cleanup_owner,
            loop_cleanup,
            cleanup_confirmed,
        } = match task_context {
            Some(context) => context,
            None => DelegateTaskContext::new()?,
        };
        let (mut child_turn, child_config) = {
            let source = child_source;
            let allowed_tools = (!inherit_all).then(|| {
                agent_config
                    .allowed_tools
                    .iter()
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .chain(std::iter::once(
                        crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME.to_owned(),
                    ))
                    .collect()
            });
            let (service, config) = {
                let DelegateChildAuthority { publication, caller } = &source.authority;
                let owner = cleanup_owner
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("ordinary delegate cleanup owner missing"))?;
                let service = publication.child_service(caller, allowed_tools, None, &agent_config.allowed_tools)?;
                owner.retain_child_service(Arc::clone(&service))?;
                (service, Arc::clone(&publication.generation().effective))
            };
            let admitted = try_boxed_delegate_field(|| {
                Ok(crate::runtime::child_turn::ChildSegmentTurn::admit(
                    service,
                    (source.proof, source.execution),
                    source.fabric,
                    Arc::clone(&config),
                    cleanup_owner
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("delegate child cleanup owner missing"))?,
                    &cancellation,
                    None,
                ))
            })?
            .await?;
            // Drop the admission future before attaching its cleanup receiver.
            let turn = admitted.with_child_cleanup(cleanup_confirmed);
            (turn, config)
        };
        let identity_path = agent_config
            .identity_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|directory| security.workspace_dir.join(directory));
        let role_prompt =
            crate::identity::delegated::compose(identity_path.as_deref(), agent_config.system_prompt.as_deref());
        let mut history = {
            let turn = &mut child_turn;
            let prompt = turn.system_prompt(&child_config, &role_prompt)?;
            let history = turn.open_first_input(Vec::new(), prompt, Some(hooks.as_ref())).await?;
            turn.project_history(history.len() + 1);
            history
        };
        // R9/R26: an enabled engine analyses the delegated child turn on its
        // own runtime context and accounting scope.
        let advice = {
            let routing = child_turn.routing();
            routing
                .service
                .advisory()
                .analyze_turn(routing.runtime, &cancellation)
                .await
        };
        let child_events = crate::hooks::ChildRun::started(
            &envelope.session_key,
            envelope.run_id.as_deref(),
            "delegate",
            Some(agent_name),
            history.last().map_or(0, |message| message.content.chars().count()),
        );
        let result = loop_cleanup
            .run(Box::pin(crate::agent::turn_engine::run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    advice,
                    channel_name: "delegate",
                    scope: Some(&scope_ctx),
                    mode: crate::agent::turn_contracts::ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &child_config.multimodal,
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: vec![
                            "sessions_spawn".to_string(),
                            "delegate".to_string(),
                            "cron".to_string(),
                        ],
                    },
                    compaction: Some(child_turn.compaction()),
                    decision: child_turn.routing(),
                },
                crate::agent::turn_request::TurnServices {
                    provider,
                    provider_name: effective_provider,
                    model: effective_model,
                    temperature,
                    tools_registry: registry,
                    observer: observer.as_ref(),
                    hooks: hooks.as_ref(),
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(child_turn.adapter()),
                    memory: crate::agent::turn_request::ToolLoopMemory::new(
                        &child_memory,
                        &security.workspace_dir,
                        Some(
                            DocumentIngestRuntime::from_scope(child_memory.clone(), &scope_ctx)
                                .with_child_partition(envelope.agent_partition.as_ref())
                                .into_nested_turn(),
                        ),
                    )
                    .with_event_fabric(child_event_fabric),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: Some(cancellation.clone()),
                    on_delta: None,
                    on_tool_call: None,
                },
            )))
            .await;
        match &result {
            Ok((outcome, _)) => child_events.finished(match outcome {
                crate::agent::turn_contracts::ToolLoopOutcome::Text(text) => text.chars().count(),
                crate::agent::turn_contracts::ToolLoopOutcome::AwaitingApproval { reply } => reply.chars().count(),
                crate::agent::turn_contracts::ToolLoopOutcome::Silent { .. } => 0,
            }),
            Err(error) => child_events.failed(
                cancellation.is_cancelled() || crate::agent::turn_contracts::is_tool_loop_cancelled(error),
                &error.to_string(),
            ),
        }
        let result = match result {
            Ok((outcome, trace)) => {
                child_turn.project_history(history.len());
                boxed_delegate_field(|| child_turn.complete(&outcome, &trace)).await?;
                Ok((outcome.into_text(), trace))
            }
            Err(error) => {
                boxed_delegate_field(|| {
                    child_turn.fail(
                        &error,
                        cancellation.is_cancelled() || crate::agent::turn_contracts::is_tool_loop_cancelled(&error),
                    )
                })
                .await?;
                if error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>() {
                    return Err(error);
                }
                Err(error)
            }
        };

        match result {
            Ok((response, trace)) => {
                let rendered = if response.trim().is_empty() {
                    "[Empty response]".to_string()
                } else {
                    response
                };

                Ok(DelegateAgenticExecution {
                    child_terminal_committed: true,
                    result: ToolResult {
                        success: true,
                        output: format!(
                            "[Agent '{agent_name}' ({provider}/{model}{mode})]\n{rendered}",
                            mode = if agent_config.agentic { ", agentic" } else { "" },
                            provider = providers::redact_endpoint_userinfo(effective_provider),
                            model = effective_model
                        ),
                        error: None,
                    },
                    trace: Some(trace),
                })
            }
            Err(e) => Ok(DelegateAgenticExecution {
                child_terminal_committed: true,
                result: ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Agent '{agent_name}' failed: {e}")),
                },
                trace: None,
            }),
        }
    }
}

struct ToolArcRef {
    inner: Arc<dyn Tool>,
    public_spec: Option<super::traits::ToolSpec>,
}

impl ToolArcRef {
    fn new(inner: Arc<dyn Tool>) -> Self {
        Self {
            inner,
            public_spec: None,
        }
    }

    fn for_name(inner: Arc<dyn Tool>, public_name: &str) -> Option<Self> {
        let public_spec = inner.spec_for_name(public_name)?;
        Some(Self {
            inner,
            public_spec: Some(public_spec),
        })
    }
}

#[async_trait]
impl Tool for ToolArcRef {
    async fn execute_named_observed(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<crate::tools::traits::ToolInvocationResult> {
        if !self.supports_name(name) {
            anyhow::bail!("Tool '{}' does not handle name '{name}'", self.name());
        }
        self.inner.execute_named_observed(name, args, cancellation).await
    }

    fn live_hook_manager(&self) -> Option<Arc<HookManager>> {
        self.inner.live_hook_manager()
    }

    fn name(&self) -> &str {
        self.public_spec
            .as_ref()
            .map_or_else(|| self.inner.name(), |spec| spec.name.as_str())
    }

    fn description(&self) -> &str {
        self.public_spec
            .as_ref()
            .map_or_else(|| self.inner.description(), |spec| spec.description.as_str())
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.public_spec
            .as_ref()
            .map_or_else(|| self.inner.parameters_schema(), |spec| spec.parameters.clone())
    }

    fn execution_parameters_schema(&self) -> Option<Arc<serde_json::Value>> {
        (self.name() == self.inner.name())
            .then(|| self.inner.execution_parameters_schema())
            .flatten()
    }

    fn normalize_arguments(&self, arguments: &mut serde_json::Value) {
        if self.name() == self.inner.name() {
            self.inner.normalize_arguments(arguments);
        }
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if let Some(spec) = &self.public_spec {
            self.inner.execute_named(&spec.name, args).await
        } else {
            self.inner.execute(args).await
        }
    }

    async fn execute_with_cancellation(
        &self,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        if let Some(spec) = &self.public_spec {
            self.inner
                .execute_named_with_cancellation(&spec.name, args, cancellation)
                .await
        } else {
            self.inner.execute_with_cancellation(args, cancellation).await
        }
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        self.inner.refresh().await
    }

    fn supports_name(&self, name: &str) -> bool {
        self.public_spec
            .as_ref()
            .map_or_else(|| self.inner.supports_name(name), |spec| spec.name == name)
    }

    async fn set_active_recipient(&self, recipient: &str) {
        self.inner.set_active_recipient(recipient).await;
    }

    async fn set_active_channel(&self, channel: Arc<dyn crate::channels::traits::Channel>) {
        self.inner.set_active_channel(channel).await;
    }

    fn specs(&self) -> Vec<super::traits::ToolSpec> {
        self.public_spec
            .as_ref()
            .map_or_else(|| self.inner.specs(), |spec| vec![spec.clone()])
    }

    fn spec_for_name(&self, name: &str) -> Option<super::traits::ToolSpec> {
        if self.public_spec.as_ref().is_some_and(|spec| spec.name != name) {
            return None;
        }
        self.inner.spec_for_name(name)
    }

    fn catalog_specs(&self) -> Vec<super::traits::ToolSpec> {
        self.public_spec.as_ref().map_or_else(
            || self.inner.catalog_specs(),
            |spec| self.inner.spec_for_name(&spec.name).into_iter().collect(),
        )
    }

    fn immutable_catalog_specs(&self) -> Option<&[super::traits::ToolSpec]> {
        match &self.public_spec {
            Some(_) => None,
            None => self.inner.immutable_catalog_specs(),
        }
    }

    fn shared_catalog_snapshot(&self) -> Option<super::SharedToolCatalogSnapshot> {
        let snapshot = self.inner.shared_catalog_snapshot()?;
        Some(match &self.public_spec {
            Some(spec) => snapshot.restrict_to(&spec.name, spec.name == self.inner.name()),
            None => snapshot,
        })
    }

    fn catalog_binding_snapshot(&self) -> std::collections::BTreeMap<String, [u8; 32]> {
        let mut bindings = self.inner.catalog_binding_snapshot();
        if let Some(spec) = &self.public_spec {
            // A selected root retains its synthetic dispatch bindings. An
            // alias projection exposes only that exact alias identity.
            if spec.name != self.inner.name() {
                bindings.retain(|name, _| name == &spec.name);
            }
        }
        bindings
    }

    fn catalog_availability_is_configured(&self) -> bool {
        self.inner.catalog_availability_is_configured()
    }

    async fn resolve_approval_operation(
        &self,
        args: &serde_json::Value,
    ) -> anyhow::Result<Option<(String, crate::acl::approval_grant::RiskLevel)>> {
        // The existing resolver is root-scoped and cannot resolve an alias
        // without a public-name argument.
        if self.name() != self.inner.name() {
            return Ok(None);
        }
        self.inner.resolve_approval_operation(args).await
    }

    async fn path_precheck(&self, args: &serde_json::Value) -> Result<(), crate::security::PathRefusal> {
        // Same root scoping as the approval resolver above.
        if self.name() != self.inner.name() {
            return Ok(());
        }
        self.inner.path_precheck(args).await
    }

    fn tier(&self) -> ToolTier {
        self.inner.tier()
    }

    fn categories(&self) -> &'static [ToolCategory] {
        self.inner.categories()
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        self.inner.availability()
    }

    async fn execute_named(&self, name: &str, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.supports_name(name) {
            anyhow::bail!("Tool '{}' does not handle name '{name}'", self.name());
        }
        self.inner.execute_named(name, args).await
    }

    async fn execute_named_with_cancellation(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        if !self.supports_name(name) {
            anyhow::bail!("Tool '{}' does not handle name '{name}'", self.name());
        }
        self.inner
            .execute_named_with_cancellation(name, args, cancellation)
            .await
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

#[cfg(all(test, unix))]
#[path = "delegate_diagnostic.rs"]
mod delegate_diagnostic;

#[cfg(test)]
mod task_boundary_tests {
    use super::{DelegateTask, DelegateTaskContext};
    use std::sync::Arc;

    #[tokio::test]
    async fn abort_before_first_poll_retains_parent_cleanup_until_drain() {
        let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        owner
            .scope(async {
                let context = DelegateTaskContext::new().expect("child task context");
                let entered_child = Arc::clone(&entered);
                let task = DelegateTask(crate::agent::channel_approval::spawn(async move {
                    let _context = context;
                    entered_child.store(true, std::sync::atomic::Ordering::SeqCst);
                    std::future::pending().await
                }));
                // This current-thread test has not yielded since spawn.
                drop(task);
                owner
                    .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
                    .await
                    .expect("aborted unpolled child cleanup acknowledged");
            })
            .await;
        assert!(!entered.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn abort_pending_child_drops_work_before_cleanup_confirmation() {
        struct WorkDropped(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for WorkDropped {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        owner
            .scope(async {
                let context = DelegateTaskContext::new().expect("child task context");
                let work_dropped = WorkDropped(Arc::clone(&dropped));
                let (entered, started) = tokio::sync::oneshot::channel();
                let task = DelegateTask(crate::agent::channel_approval::spawn(async move {
                    context
                        .loop_cleanup
                        .run(async move {
                            let _work_dropped = work_dropped;
                            entered.send(()).expect("parent remains waiting");
                            std::future::pending().await
                        })
                        .await
                }));
                started.await.expect("child was polled");
                drop(task);
                owner
                    .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
                    .await
                    .expect("aborted polled child cleanup acknowledged");
            })
            .await;
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
pub(crate) mod tests;

/// C0-C: the model may name a provider per call, but only one the operator
/// configured, and each provider receives only its own credential. Driven
/// through `DelegateTool::execute`, the real provider factory and loopback
/// endpoints; every credential is a sentinel.
#[cfg(test)]
mod credential_containment_tests;

#[cfg(test)]
mod result_contract_tests {
    use super::{DelegateTool, delegate_provider_failure};
    use crate::providers::catalog::ResolutionError;
    use crate::security::SecurityPolicy;
    use crate::tools::traits::Tool;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn missing_parent_returns_unsuccessful_result() -> anyhow::Result<()> {
        let delegate = DelegateTool::new(HashMap::new(), Arc::new(SecurityPolicy::default()));
        let result = delegate
            .execute(serde_json::json!({"agent": "prx_agent", "prompt": "task"}))
            .await?;
        assert!(!result.success);
        assert!(result.output.is_empty(), "refused delegation produces no output");
        assert_eq!(
            result.error.as_deref(),
            Some("delegation requires an admitted parent source")
        );
        Ok(())
    }

    #[test]
    fn unsupported_provider_returns_unsuccessful_result() -> anyhow::Result<()> {
        let refusal = ResolutionError::UnknownProvider {
            provider: "prx_unsupported".into(),
        };
        let expected = refusal.to_string();
        let result = delegate_provider_failure(refusal.into())?;
        assert!(!result.success);
        assert!(result.output.is_empty(), "refused delegation produces no output");
        assert_eq!(result.error.as_deref(), Some(expected.as_str()));
        Ok(())
    }

    #[test]
    fn credential_and_selection_errors_preserve_typed_failure() {
        let errors = [
            ResolutionError::CredentialOwnerMismatch {
                provider: "prx_provider".into(),
                reference: "profile:prx_profile".into(),
            },
            ResolutionError::CredentialMissing {
                provider: "prx_provider".into(),
                reference: "profile:prx_profile".into(),
            },
            ResolutionError::CredentialRequired {
                provider: "prx_provider".into(),
            },
            ResolutionError::CredentialLookup {
                provider: "prx_provider".into(),
                reason: "unavailable".into(),
            },
            ResolutionError::UnknownRouteHint {
                provider: "prx_provider".into(),
                hint: "prx_route".into(),
                configured: String::new(),
            },
            ResolutionError::ModelIncompatible {
                provider: "prx_provider".into(),
                model: "prx_model".into(),
                detail: "incompatible".into(),
            },
        ];
        for refusal in errors {
            let actual = delegate_provider_failure(refusal.clone().into());
            assert!(matches!(actual, Err(error) if error.downcast_ref::<ResolutionError>() == Some(&refusal)));
        }
    }

    #[test]
    fn opaque_integrity_and_factory_errors_remain_errors() {
        for message in [
            "delegate caller authority mismatch",
            "ordinary child requires disabled decision plane",
            "worker provider or credential provenance differs from issued selection",
            "provider 'prx_provider' is not one PRX can construct",
            "provider factory construction failed",
        ] {
            let actual = delegate_provider_failure(anyhow::anyhow!(message));
            assert!(matches!(actual, Err(error) if error.to_string() == message));
        }
    }
}
