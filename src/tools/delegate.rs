use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_contracts::ScopeContext;
use crate::config::DelegateAgentConfig;
use crate::hooks::HookManager;
use crate::memory::{Memory, MemoryEventRecording, MemoryFabric, MessageEventScope};
use crate::observability::traits::{Observer, ObserverEvent, ObserverMetric};
use crate::providers::{self, ChatMessage, Provider};
use crate::router::CompactionResolver;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::SecurityPolicy;
use crate::security::policy::ToolOperation;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

#[expect(
    clippy::large_enum_variant,
    reason = "Inline child context preserves ownership without adding a hot-path allocation"
)]
enum DelegateChildAuthority {
    Granted(Arc<crate::runtime::child_source::IssuedChildGrant>),
    Ordinary {
        publication: crate::runtime::turn_decision::OrdinaryChildPublication,
        caller: crate::tools::ToolExecutionContext,
    },
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
    /// Inherited multimodal handling config for sub-agent loops.
    multimodal_config: crate::config::MultimodalConfig,
    /// Model-aware compaction resolver inherited from root config.
    compaction_resolver: CompactionResolver,
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
        Self::new_with_options(agents, security, providers::ProviderRuntimeOptions::default())
    }

    pub fn new_with_options(
        agents: HashMap<String, DelegateAgentConfig>,
        security: Arc<SecurityPolicy>,
        _provider_runtime_options: providers::ProviderRuntimeOptions,
    ) -> Self {
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
            multimodal_config: crate::config::MultimodalConfig::default(),
            compaction_resolver: CompactionResolver::from_base(crate::config::AgentCompactionConfig::default()),
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
        Self::with_depth_and_options(agents, security, depth, providers::ProviderRuntimeOptions::default())
    }

    pub fn with_depth_and_options(
        agents: HashMap<String, DelegateAgentConfig>,
        security: Arc<SecurityPolicy>,
        depth: u32,
        _provider_runtime_options: providers::ProviderRuntimeOptions,
    ) -> Self {
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
            multimodal_config: crate::config::MultimodalConfig::default(),
            compaction_resolver: CompactionResolver::from_base(crate::config::AgentCompactionConfig::default()),
            memory: None,
            event_recording: MemoryEventRecording::default(),
        }
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

    /// Attach model-aware compaction resolution for agentic sub-agent loops.
    pub fn with_compaction_resolver(mut self, resolver: CompactionResolver) -> Self {
        self.compaction_resolver = resolver;
        self
    }

    fn resolve_child_compaction(
        &self,
        provider: &str,
        model: &str,
    ) -> crate::router::context::EffectiveCompactionConfig {
        self.compaction_resolver.resolve(provider, model)
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
                    "description": "Optional provider override. Must be a provider the operator configured (a named agent's provider, the default provider, a fallback provider or a model route); any other provider, including an unconfigured 'custom:<url>', is refused. Defaults to the named agent's configured provider."
                }
            },
            "required": ["agent", "prompt"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let child_grant = crate::runtime::child_source::outgoing_grant();
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
        }
        anyhow::ensure!(
            child_grant.is_some() || ordinary_publication.is_some(),
            "delegation requires an admitted parent source"
        );
        let security = match child_grant.as_ref() {
            Some(grant) => grant.security(),
            None => ordinary_publication
                .ok_or_else(|| anyhow::anyhow!("delegate parent policy missing"))?
                .security(),
        };
        let child_config = child_grant
            .as_ref()
            .map(|grant| grant.generation().effective.as_ref())
            .or_else(|| ordinary_publication.map(|publication| publication.generation().effective.as_ref()))
            .ok_or_else(|| anyhow::anyhow!("delegate admitted configuration missing"))?;
        anyhow::ensure!(
            !child_config.causal_tree.enabled || child_grant.is_some(),
            "decision-enabled delegation requires an issued child grant"
        );
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

        let child_binding = {
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
            match child_grant.as_ref() {
                Some(grant) => crate::runtime::child_source::ChildProviderBinding::build(
                    Arc::clone(grant),
                    &effective_provider,
                    &effective_model,
                    credential,
                )?,
                None => crate::runtime::child_source::ChildProviderBinding::build_ordinary(
                    config,
                    &self.model_catalog,
                    &effective_provider,
                    &effective_model,
                    credential,
                    ordinary_publication,
                )?,
            }
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
        let memory_fabric = child_grant
            .as_ref()
            .map(|grant| grant.source_fabric().clone())
            .or_else(|| ordinary_publication.map(|publication| publication.source_fabric()))
            .ok_or_else(|| anyhow::anyhow!("delegate admitted memory fabric missing"))?;
        let mut event_scope = delegate_event_scope(
            &security.workspace_dir.to_string_lossy(),
            &delegate_run_id,
            agent_name,
            scope.as_ref(),
        );
        if let Some(grant) = child_grant.as_ref() {
            grant.bind_event_scope(&mut event_scope)?;
        } else {
            child_binding.bind_ordinary_input_scope(
                &provider,
                &security.workspace_dir.to_string_lossy(),
                &mut event_scope,
                caller.as_ref(),
            )?;
        }
        let source_receipt = crate::runtime::child_source::record_child_input(
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
            child_grant.as_deref(),
        )
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
        let child_source = if let Some(grant) = child_grant.as_ref() {
            let receipt = source_receipt
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("delegate child input is not durable"))?;
            let proof = Arc::new(child_binding.source_proof(grant, receipt, &provider)?);
            let execution = receipt.execution_context(proof.as_ref())?;
            Some(DelegateChildSource {
                authority: DelegateChildAuthority::Granted(Arc::clone(grant)),
                proof,
                execution,
                fabric: memory_fabric.clone(),
            })
        } else if let Some(caller) = caller.as_ref()
            && let Some(publication) = &caller.context().ordinary_child_publication
        {
            let (proof, execution) = child_binding.ordinary_execution_source(
                &provider,
                caller,
                &event_scope,
                &full_prompt,
                source_receipt.as_deref(),
            )?;
            Some(DelegateChildSource {
                authority: DelegateChildAuthority::Ordinary {
                    publication: publication.clone(),
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
            .execute_tool_loop(
                agent_name,
                agent_config,
                &effective_provider,
                &effective_model,
                &*provider,
                &full_prompt,
                temperature,
                scope.as_ref(),
                Some(child_source),
            )
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
            record_delegate_result_event(&memory_fabric, event_scope.clone(), &result).await;
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

impl DelegateTool {
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
    ) -> anyhow::Result<DelegateAgenticExecution> {
        let security = match child_source.as_ref().map(|source| &source.authority) {
            Some(DelegateChildAuthority::Ordinary { publication, .. }) => Arc::clone(publication.security()),
            Some(DelegateChildAuthority::Granted(grant)) => Arc::clone(grant.security()),
            _ => Arc::clone(&self.security),
        };
        if let Some(source) = child_source.as_ref() {
            use crate::runtime::turn_source::TurnSourceProof;
            anyhow::ensure!(
                source.proof.routing_input() == full_prompt,
                "delegate execution input differs from its admitted source"
            );
        }
        let answer_only = child_source.is_some() && !agent_config.agentic;
        if !answer_only && agent_config.allowed_tools.is_empty() {
            return Ok(DelegateAgenticExecution {
                child_terminal_committed: false,
                result: ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Agent '{agent_name}' has agentic=true but allowed_tools is empty"
                    )),
                },
                trace: None,
            });
        }

        let requested = agent_config
            .allowed_tools
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .collect::<std::collections::HashSet<_>>();
        let inherit_all = answer_only || requested.contains("*");
        if !answer_only && inherit_all && requested.len() != 1 {
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
            // Granted execution always uses exact per-call bindings and the
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
            && child_source.as_ref().is_none_or(|source| match &source.authority {
                DelegateChildAuthority::Granted(grant) => grant
                    .catalog()
                    .entry_for_public_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME)
                    .is_some_and(|entry| {
                        grant
                            .scope()
                            .gate_inputs()
                            .0
                            .allowed
                            .contains(&crate::causal_tree::branch::CapabilityRef {
                                id: entry.id.clone(),
                                fingerprint: entry.fingerprint,
                                generation: entry.generation,
                            })
                    }),
                DelegateChildAuthority::Ordinary { publication, .. } => {
                    publication.allows_tool(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME)
                }
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

        let (hooks, observer): (Arc<HookManager>, Arc<dyn Observer>) =
            match child_source.as_ref().map(|source| &source.authority) {
                Some(DelegateChildAuthority::Ordinary { publication, .. }) => {
                    let runtime = publication.loop_runtime()?;
                    (Arc::clone(&runtime.hooks), Arc::clone(&runtime.observer))
                }
                _ => (
                    Arc::new(HookManager::new(security.workspace_dir.clone())),
                    Arc::new(NoopObserver),
                ),
            };
        let child_source =
            child_source.ok_or_else(|| anyhow::anyhow!("delegate requires its admitted child source"))?;
        let child_execution = child_source.execution.clone();
        let child_event_fabric = child_source.fabric.clone();
        let child_memory = child_event_fabric.memory();
        let envelope = &child_execution.envelope;
        let scope_ctx = ScopeContext {
            policy: &security,
            sender: envelope.sender.as_deref().unwrap_or_default(),
            channel: envelope.channel.as_deref().unwrap_or_default(),
            chat_type: &child_execution.chat_type,
            chat_id: &envelope.session_key,
            owner_id: envelope.owner_id.as_deref(),
            topic_id: envelope.topic_id.as_deref(),
            task_id: envelope.task_id.as_deref(),
            source_message_event_id: envelope.source_message_event_id.as_deref(),
            config_generation_id: envelope.config_generation_id,
            config_source_revision: envelope.config_source_revision.as_deref(),
        };
        let resolved_compaction = self.resolve_child_compaction(effective_provider, effective_model);
        tracing::debug!(
            agent = agent_name,
            provider = %resolved_compaction.shown_provider(),
            model = resolved_compaction.selected_model.as_str(),
            max_context_tokens = resolved_compaction.config.max_context_tokens,
            source = ?resolved_compaction.max_context_source,
            kernel_capped = resolved_compaction.kernel_capped,
            "delegate resolved child compaction window"
        );

        let registry = Arc::new(sub_tools);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cleanup_owner = crate::runtime::turn_cleanup::current_tracker();
        let (cleanup_ack, cleanup_confirmed) = tokio::sync::oneshot::channel();
        let loop_cleanup =
            crate::runtime::turn_cleanup::CleanupScope::new(cleanup_owner.as_ref(), cancellation.clone())?
                .with_acknowledgement(Some(cleanup_ack));
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
            let (service, config) = match &source.authority {
                DelegateChildAuthority::Granted(grant) => {
                    let service = crate::runtime::turn_publication::ChildDecisionPublication {
                        allowed_tools,
                        grant: Arc::clone(grant),
                        registry: Arc::clone(&registry),
                        observer: Arc::new(NoopObserver),
                    }
                    .publish()
                    .await?;
                    (service, Arc::clone(&grant.generation().effective))
                }
                DelegateChildAuthority::Ordinary { publication, caller } => {
                    let owner = cleanup_owner
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("ordinary delegate cleanup owner missing"))?;
                    let service = publication.child_service(caller, allowed_tools)?;
                    owner.retain_child_service(Arc::clone(&service))?;
                    (service, Arc::clone(&publication.generation().effective))
                }
            };
            let turn = crate::runtime::child_turn::ChildSegmentTurn::admit(
                service,
                (source.proof, source.execution),
                source.fabric,
                Arc::clone(&config),
                cleanup_owner
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("delegate child cleanup owner missing"))?,
                &cancellation,
                None,
            )
            .await?
            .with_child_cleanup(cleanup_confirmed);
            (turn, config)
        };
        let mut history = Vec::new();
        if let Some(system_prompt) = agent_config.system_prompt.as_ref() {
            history.push(ChatMessage::system(system_prompt.clone()));
        }
        history.push(crate::agent::turn_contracts::inbound_user_message(full_prompt));

        {
            let turn = &mut child_turn;
            turn.project_history(history.len() + 1);
            if turn.is_close()? {
                loop_cleanup.run(async { Ok(()) }).await?;
                let reply = turn
                    .close(&cancellation)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("delegate close disposition changed"))?;
                return Ok(DelegateAgenticExecution {
                    child_terminal_committed: true,
                    result: ToolResult {
                        success: true,
                        output: reply,
                        error: None,
                    },
                    trace: None,
                });
            }
            let prompt = turn.prompt_with_skills(agent_config.system_prompt.as_deref().unwrap_or_default())?;
            if let Some(system) = history.first_mut().filter(|message| message.role == "system") {
                *system = ChatMessage::system(prompt);
            } else if !prompt.is_empty() {
                history.insert(0, ChatMessage::system(prompt));
            }
            turn.project_history(history.len() + 1);
            turn.enrich_input(&mut history)?;
        }
        let result = loop_cleanup
            .run(crate::agent::turn_engine::run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
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
                        Some(DocumentIngestRuntime::from_scope(child_memory.clone(), &scope_ctx).into_nested_turn()),
                    )
                    .with_event_fabric(child_event_fabric),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: Some(cancellation.clone()),
                    on_delta: None,
                    on_tool_call: None,
                },
            ))
            .await;
        let result = match result {
            Ok((outcome, trace)) => {
                child_turn.project_history(history.len());
                child_turn.complete(&outcome, &trace).await?;
                Ok((outcome.into_text(), trace))
            }
            Err(error) => {
                child_turn
                    .fail(
                        &error,
                        cancellation.is_cancelled() || crate::agent::turn_contracts::is_tool_loop_cancelled(&error),
                    )
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

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryPrincipal, SqliteMemory};
    use crate::providers::{ChatRequest, ChatResponse, ToolCall};
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Workspace for the delegate tests, unique per process run.
    ///
    /// The tool-execution ledger lives inside the workspace and is durable on
    /// purpose, so the default `.` workspace would both write into the source
    /// tree and let one run's records replay into the next.
    fn test_workspace() -> std::path::PathBuf {
        static WORKSPACE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        WORKSPACE
            .get_or_init(|| {
                std::env::temp_dir().join(format!(
                    "prx-delegate-tests-{}-{}",
                    std::process::id(),
                    uuid::Uuid::now_v7()
                ))
            })
            .clone()
    }

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            workspace_dir: test_workspace(),
            ..SecurityPolicy::default()
        })
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
                credential_ref: None,
                temperature: Some(0.3),
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
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
                credential_ref: None,
                temperature: None,
                max_depth: 2,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );
        agents
    }

    /// Counts its own executions, so a test can tell "the delegated sub-agent ran
    /// the tool" from "the tool was refused and the refusal was handed back as
    /// tool output".
    #[derive(Default)]
    struct EchoTool {
        executions: Arc<AtomicUsize>,
    }

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
            self.executions.fetch_add(1, Ordering::SeqCst);
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

    struct DynamicAliasTool;

    #[async_trait]
    impl Tool for DynamicAliasTool {
        fn name(&self) -> &str {
            "dynamic_root"
        }

        fn description(&self) -> &str {
            "Dynamic alias fixture"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        fn specs(&self) -> Vec<super::super::traits::ToolSpec> {
            vec![super::super::traits::ToolSpec {
                name: "dynamic_alias".to_string(),
                description: self.description().to_string(),
                parameters: self.parameters_schema(),
            }]
        }

        fn supports_name(&self, name: &str) -> bool {
            matches!(name, "dynamic_root" | "dynamic_alias")
        }

        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.execute_named("dynamic_root", args).await
        }

        async fn execute_named(&self, name: &str, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: self.supports_name(name),
                output: name.to_string(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn delegated_tool_reference_preserves_dynamic_alias_contract() {
        let delegated = ToolArcRef::new(Arc::new(DynamicAliasTool));
        assert!(delegated.supports_name("dynamic_alias"));
        assert_eq!(delegated.specs()[0].name, "dynamic_alias");
        let result = delegated.execute_named("dynamic_alias", json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "dynamic_alias");

        let restricted = ToolArcRef::for_name(Arc::new(DynamicAliasTool), "dynamic_alias").unwrap();
        assert_eq!(restricted.name(), "dynamic_alias");
        assert_eq!(restricted.specs().len(), 1);
        assert!(restricted.supports_name("dynamic_alias"));
        assert!(!restricted.supports_name("dynamic_root"));
        assert!(restricted.execute_named("dynamic_root", json!({})).await.is_err());
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

    fn agentic_config(allowed_tools: Vec<String>) -> DelegateAgentConfig {
        DelegateAgentConfig {
            provider: "openrouter".to_string(),
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: None,
            model: "model-test".to_string(),
            system_prompt: Some("You are agentic.".to_string()),
            api_key: Some("delegate-test-credential".to_string()),
            credential_ref: None,
            temperature: Some(0.2),
            max_depth: 3,
            agentic: true,
            allowed_tools,
        }
    }

    fn router_model(provider: &str, model_id: &str, max_context: usize) -> crate::config::RouterModelConfig {
        crate::config::RouterModelConfig {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            cost_per_million_tokens: 1.0,
            max_context,
            reserved_output_tokens: None,
            latency_ms: 1_000,
            categories: vec!["code".to_string()],
            elo_rating: 1_000.0,
        }
    }

    fn compaction_resolver_with_models(
        models: Vec<crate::config::RouterModelConfig>,
    ) -> crate::router::CompactionResolver {
        let mut router = crate::config::RouterConfig::default();
        router.models = models;
        crate::router::CompactionResolver::new(crate::config::AgentCompactionConfig::default(), router, Vec::new())
    }

    #[test]
    fn name_and_schema() {
        let tool = DelegateTool::new(sample_agents(), test_security());
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
        let tool = DelegateTool::new(sample_agents(), test_security());
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn schema_lists_agent_names() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let schema = tool.parameters_schema();
        let desc = schema["properties"]["agent"]["description"].as_str().unwrap();
        assert!(desc.contains("researcher") || desc.contains("coder"));
    }

    #[tokio::test]
    async fn missing_agent_param() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = tool.execute(json!({"prompt": "test"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_prompt_param() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = tool.execute(json!({"agent": "researcher"})).await;
        assert!(result.is_err());
    }

    async fn dispatch_configured_delegate(mut tool: DelegateTool, arguments: serde_json::Value) -> ToolResult {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = false;
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = tool.agents.as_ref().clone();
        tool.security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
        dispatch_admitted_delegate(config, tool, arguments).await
    }

    #[tokio::test]
    async fn unknown_agent_returns_error() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = dispatch_configured_delegate(tool, json!({"agent":"nonexistent", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown agent"));
    }

    #[tokio::test]
    async fn depth_limit_enforced() {
        let tool = DelegateTool::with_depth(sample_agents(), test_security(), 3);
        let result = dispatch_configured_delegate(tool, json!({"agent":"researcher", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("depth limit"));
    }

    #[tokio::test]
    async fn depth_limit_per_agent() {
        // coder has max_depth=2, so depth=2 should be blocked
        let tool = DelegateTool::with_depth(sample_agents(), test_security(), 2);
        let result = dispatch_configured_delegate(tool, json!({"agent":"coder", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("depth limit"));
    }

    #[test]
    fn empty_agents_schema() {
        let tool = DelegateTool::new(HashMap::new(), test_security());
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
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );
        let tool = DelegateTool::new(agents, test_security());
        let result = dispatch_configured_delegate(tool, json!({"agent":"broken", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Failed to create provider"));
    }

    #[tokio::test]
    async fn blank_agent_rejected() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = tool.execute(json!({"agent": "  ", "prompt": "test"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn blank_prompt_rejected() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = tool
            .execute(json!({"agent": "researcher", "prompt": "  \t  "}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn whitespace_agent_name_trimmed_and_found() {
        let tool = DelegateTool::new(single_agent("researcher", "mock", "test-model"), test_security());
        let result = dispatch_configured_delegate(tool, json!({"agent":" researcher ", "prompt":"test"})).await;
        assert!(result.success, "trimmed agent must execute: {:?}", result.error);
        assert!(result.output.contains("mock/test-model"));
    }

    #[tokio::test]
    async fn delegation_blocked_in_readonly_mode() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = false;
        config.autonomy.level = AutonomyLevel::ReadOnly;
        config.agents = single_agent("researcher", "mock", "test-model");
        let readonly = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
        let tool = DelegateTool::new(config.agents.clone(), readonly);
        let (result, executions, history) =
            observe_delegate_dispatch(config, tool, json!({"agent":"researcher", "prompt":"test"}), None).await;
        assert_eq!(executions, 0, "readonly execution must reject before entering delegate");
        assert!(result.is_none());
        assert!(
            history.iter().any(|message| message.role == "user"
                && message
                    .content
                    .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX)
                && message
                    .content
                    .contains("tool 'delegate' is not permitted under the current execution policy")),
            "the parent must receive the actual policy denial: {history:?}"
        );
    }

    #[tokio::test]
    async fn delegate_context_is_prepended_to_prompt() {
        let (result, requests) = run_admitted_delegate_fixture(
            vec!["dynamic_alias".to_owned()],
            vec![Arc::new(DynamicAliasTool)],
            "dynamic_alias",
            json!({}),
            None,
            false,
            Some("some context data"),
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(requests.len(), 2, "child must execute the tool and finish");
        let messages = requests[0]["messages"].as_array().unwrap();
        let user_message = messages
            .iter()
            .find(|message| message["role"] == "user")
            .expect("the provider must receive the delegated user input");
        assert_eq!(
            user_message["content"],
            "[Context]\nsome context data\n\n[Task]\ndynamic_alias"
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
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );
        let mut config = crate::config::Config::default();
        config.workspace_dir = tmp.path().to_owned();
        config.config_path = tmp.path().join("config.toml");
        config.causal_tree.enabled = false;
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = agents.clone();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, tmp.path()));
        let tool = DelegateTool::new(agents, security).with_shared_memory(memory.clone());
        let channel = crate::channels::traits::ChannelMessage {
            id: "prx-source-message".to_owned(),
            sender: "prx_user".to_owned(),
            reply_target: "prx_channel".to_owned(),
            content: String::new(),
            channel: "telegram".to_owned(),
            timestamp: 0,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            thread_ts: None,
            mentioned_uuids: Vec::new(),
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        let result = dispatch_admitted_delegate_from(
            config,
            tool,
            json!({"agent":"tester", "prompt":"delegate through fabric"}),
            Some(channel),
        )
        .await;
        assert!(result.success);

        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: Some("tester".to_string()),
                    persona_id: None,
                    session_key: None,
                    channel: Some("telegram".to_string()),
                    sender: Some("prx_user".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();

        let request_event = events
            .iter()
            .find(|event| event.role == "user")
            .expect("delegate request event");
        let result_event = events
            .iter()
            .find(|event| event.role == "event" && event.content.contains("tester"))
            .expect("delegate domain result event");
        assert_eq!(request_event.source, "delegate");
        assert_eq!(request_event.content, "delegate through fabric");
        assert_eq!(result_event.source, "delegate");
        assert_eq!(events.iter().filter(|event| event.role == "assistant").count(), 1);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "provider.final_outcome")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );

        let parent_id = request_event
            .causation_event_id
            .as_deref()
            .expect("delegate input must reference the actual parent source event");
        let parent_events = memory
            .list_message_events_recent(
                &MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: None,
                    channel: Some("telegram".to_owned()),
                    sender: Some("prx_user".to_owned()),
                    owner_id: None,
                    legacy_session_key: None,
                },
                100,
            )
            .await
            .unwrap();
        let parent = parent_events
            .iter()
            .find(|event| event.event_id == parent_id)
            .expect("the referenced source event must actually be durable");
        assert_eq!(parent.role, "user");
        assert_eq!(parent.content, "delegate delegate through fabric");
        assert_eq!(request_event.parent_run_id, parent.run_id);
        assert_eq!(request_event.parent_session_key, parent.session_key);

        let task_events = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: Some("tester".to_string()),
                    persona_id: None,
                    session_key: None,
                    channel: Some("telegram".to_string()),
                    sender: Some("prx_user".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
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
            event.payload_json.as_deref().is_some_and(|payload| {
                serde_json::from_str::<serde_json::Value>(payload)
                    .is_ok_and(|value| value["source_message_event_id"] == parent_id)
            })
        }));
    }

    #[test]
    fn delegate_event_scope_is_derived_from_runtime_envelope() {
        let scope = DelegateScope {
            sender: "alice".to_string(),
            channel: "telegram".to_string(),
            chat_id: "chat-1".to_string(),
            owner_id: Some("owner:/tmp/ws:telegram:alice".to_string()),
            topic_id: Some("topic-a".to_string()),
            task_id: Some("task-a".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
            config_generation_id: Some(17),
            config_source_revision: Some("revision-17".to_string()),
        };
        let event_scope = delegate_event_scope("/tmp/ws", "run-delegate", "tester", Some(&scope));

        assert_eq!(event_scope.source, "delegate");
        assert_eq!(event_scope.channel.as_deref(), Some("telegram"));
        assert_eq!(
            event_scope.session_key.as_deref(),
            Some("delegate:telegram:chat-1:alice")
        );
        assert_eq!(event_scope.run_id.as_deref(), Some("run-delegate"));
        assert_eq!(event_scope.config_generation_id, Some(17));
        assert_eq!(event_scope.config_source_revision.as_deref(), Some("revision-17"));
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
                "source_message_event_id": "msg-a",
                "config_generation_id": 17,
                "config_source_revision": "revision-17"
            }
        }))
        .expect("trusted scope should parse");

        assert_eq!(scope.owner_id.as_deref(), Some("owner:/tmp/ws:telegram:alice"));
        assert_eq!(scope.topic_id.as_deref(), Some("topic-a"));
        assert_eq!(scope.task_id.as_deref(), Some("task-a"));
        assert_eq!(scope.source_message_event_id.as_deref(), Some("msg-a"));
        assert_eq!(scope.config_generation_id, Some(17));
        assert_eq!(scope.config_source_revision.as_deref(), Some("revision-17"));
    }

    #[tokio::test]
    async fn delegate_empty_context_omits_prefix() {
        let (result, requests) = run_admitted_delegate_fixture(
            vec!["dynamic_alias".to_owned()],
            vec![Arc::new(DynamicAliasTool)],
            "dynamic_alias",
            json!({}),
            None,
            false,
            Some(""),
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(requests.len(), 2, "child must execute the tool and finish");
        let messages = requests[0]["messages"].as_array().unwrap();
        let user_message = messages
            .iter()
            .find(|message| message["role"] == "user")
            .expect("the provider must receive the delegated user input");
        assert_eq!(user_message["content"], "dynamic_alias");
    }

    #[test]
    fn delegate_depth_construction() {
        let tool = DelegateTool::with_depth(sample_agents(), test_security(), 5);
        assert_eq!(tool.depth, 5);
    }

    #[tokio::test]
    async fn delegate_no_agents_configured() {
        let tool = DelegateTool::new(HashMap::new(), test_security());
        let result = dispatch_configured_delegate(tool, json!({"agent":"any", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("none configured"));
    }

    #[tokio::test]
    async fn agentic_mode_rejects_empty_allowed_tools() {
        let mut agents = HashMap::new();
        agents.insert("agentic".to_string(), agentic_config(Vec::new()));

        let tool = DelegateTool::new(agents, test_security());
        let result = dispatch_configured_delegate(tool, json!({"agent":"agentic", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("allowed_tools is empty"));
    }

    #[tokio::test]
    async fn agentic_mode_rejects_unmatched_allowed_tools() {
        let mut agents = HashMap::new();
        agents.insert("agentic".to_string(), agentic_config(vec!["missing_tool".to_string()]));

        let tool =
            DelegateTool::new(agents, test_security()).with_parent_tools(Arc::new(vec![Arc::new(EchoTool::default())]));
        let result = dispatch_configured_delegate(tool, json!({"agent":"agentic", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("unknown or ineligible tools: missing_tool")
        );
    }

    struct CapturedDelegate {
        inner: DelegateTool,
        result: Arc<parking_lot::Mutex<Option<ToolResult>>>,
        executions: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CapturedDelegate {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn description(&self) -> &str {
            self.inner.description()
        }
        fn parameters_schema(&self) -> serde_json::Value {
            self.inner.parameters_schema()
        }
        fn tier(&self) -> ToolTier {
            self.inner.tier()
        }
        fn categories(&self) -> &'static [ToolCategory] {
            self.inner.categories()
        }
        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            let result = self.inner.execute(args).await?;
            *self.result.lock() = Some(result.clone());
            Ok(result)
        }
    }

    struct DelegateParentProvider {
        arguments: serde_json::Value,
    }

    #[async_trait]
    impl Provider for DelegateParentProvider {
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            anyhow::bail!("fixture must use the shared parent tool loop")
        }
        async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            let content = if request.messages.iter().any(|message| {
                message.role == "tool"
                    || (message.role == "user"
                        && message
                            .content
                            .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX))
            }) {
                "parent complete".to_owned()
            } else {
                format!(
                    "<tool_call>\n{}\n</tool_call>",
                    json!({
                        "name":"delegate", "arguments":self.arguments
                    })
                )
            };
            Ok(ChatResponse {
                text: Some(content),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
    }

    struct DelegateFixtureServer(tokio::task::JoinHandle<()>);
    impl Drop for DelegateFixtureServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    tokio::task_local! { static CONFIGURED_DELEGATE_ENDPOINT: String; }

    async fn run_configured_delegate_fixture(
        allowed_tools: Vec<String>,
        parent_tools: Vec<Arc<dyn Tool>>,
        tool_name: &'static str,
        arguments: serde_json::Value,
        provider_error: Option<&'static str>,
        custom_userinfo: bool,
        context: Option<&str>,
    ) -> (ToolResult, Vec<serde_json::Value>) {
        use futures::FutureExt as _;
        let retained = tempfile::tempdir().expect("private decision evidence").keep();
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_delegation("native_tool")
            .await
            .expect("owned compatible decision listener")
            .retain_evidence_at(retained);
        let endpoint = driver.endpoint().to_owned();
        driver
            .run_factory(true, move || async move {
                let cleanup =
                    crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                        .expect("delegate fixture cleanup owner");
                let mut observed = None;
                let drained = cleanup
                    .run(async {
                        observed = Some(
                            CONFIGURED_DELEGATE_ENDPOINT
                                .scope(
                                    endpoint,
                                    std::panic::AssertUnwindSafe(run_admitted_delegate_fixture(
                                        allowed_tools,
                                        parent_tools,
                                        tool_name,
                                        arguments,
                                        provider_error,
                                        custom_userinfo,
                                        context,
                                    ))
                                    .catch_unwind(),
                                )
                                .await,
                        );
                        Ok(())
                    })
                    .await;
                match observed {
                    Some(Err(panic)) => std::panic::resume_unwind(panic),
                    Some(Ok(value)) => {
                        drained.expect("delegate fixture writers drained");
                        value
                    }
                    None => {
                        drained.expect("delegate fixture cleanup entered");
                        panic!("delegate fixture did not run")
                    }
                }
            })
            .await
    }

    async fn run_admitted_delegate_fixture(
        allowed_tools: Vec<String>,
        parent_tools: Vec<Arc<dyn Tool>>,
        tool_name: &'static str,
        arguments: serde_json::Value,
        provider_error: Option<&'static str>,
        custom_userinfo: bool,
        context: Option<&str>,
    ) -> (ToolResult, Vec<serde_json::Value>) {
        let workspace = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let workspace = super::delegate_diagnostic::workspace(workspace);
        let requests = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
        let capture = Arc::clone(&requests);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let handler = axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| {
            let capture = Arc::clone(&capture);
            let arguments = arguments.clone();
            async move {
                let completed = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool");
                capture.lock().push(request);
                if let Some(error) = provider_error {
                    return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error":error})));
                }
                let message = if completed {
                    json!({"role":"assistant", "content":"done"})
                } else if custom_userinfo {
                    json!({"role":"assistant", "content":null, "tool_calls":[{
                        "id":"call_dynamic", "type":"function",
                        "function":{"name":tool_name, "arguments":arguments.to_string()}
                    }]})
                } else {
                    json!({"role":"assistant", "content":"", "tool_calls":[{
                        "function":{"name":tool_name, "arguments":arguments}
                    }]})
                };
                let body = if custom_userinfo {
                    json!({"id":"fixture-response", "object":"chat.completion", "created":0,
                            "model":"prx-local-model", "choices":[{"index":0, "message":message,
                            "finish_reason":if completed { "stop" } else { "tool_calls" }}],
                            "usage":{"prompt_tokens":1, "completion_tokens":1, "total_tokens":2}})
                } else {
                    json!({"model":"prx-local-model", "message":message,
                            "done":true, "prompt_eval_count":1, "eval_count":1})
                };
                (axum::http::StatusCode::OK, axum::Json(body))
            }
        });
        let app = axum::Router::new()
            .route("/api/chat", handler.clone())
            .route("/v1/chat/completions", handler);
        let mut server = DelegateFixtureServer(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.api_url = Some(endpoint.clone());
        config.default_provider = Some("ollama".to_owned());
        config.default_model = Some("prx-local-model".to_owned());
        config.causal_tree.enabled = false;
        config.reliability.provider_retries = 0;
        config.autonomy.level = AutonomyLevel::Full;
        let mut agent = agentic_config(allowed_tools);
        agent.provider = if custom_userinfo {
            format!(
                "custom:http://prx_user:DELEGATE-SECRET-pw@{}/v1",
                endpoint.trim_start_matches("http://")
            )
        } else {
            "ollama".to_owned()
        };
        agent.model = "prx-local-model".to_owned();
        config.agents.insert("tester".to_owned(), agent);
        config.cost.prices.insert(
            "prx-local-model".to_owned(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        let delegate = DelegateTool::new(
            config.agents.clone(),
            Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path())),
        )
        .with_parent_tools(Arc::new(parent_tools));
        use futures::FutureExt as _;
        let result = std::panic::AssertUnwindSafe(dispatch_admitted_delegate(
            config,
            delegate,
            json!({"agent":"tester", "prompt":tool_name, "context":context}),
        ))
        .catch_unwind()
        .await;
        server.0.abort();
        let _ = (&mut server.0).await;
        let requests = requests.lock().clone();
        match result {
            Ok(result) => (result, requests),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    pub(super) async fn dispatch_admitted_delegate(
        config: crate::config::Config,
        delegate: DelegateTool,
        arguments: serde_json::Value,
    ) -> ToolResult {
        dispatch_admitted_delegate_from(config, delegate, arguments, None).await
    }

    async fn dispatch_admitted_delegate_from(
        config: crate::config::Config,
        delegate: DelegateTool,
        arguments: serde_json::Value,
        channel: Option<crate::channels::traits::ChannelMessage>,
    ) -> ToolResult {
        let (result, executions, _) = observe_delegate_dispatch(config, delegate, arguments, channel).await;
        assert_eq!(executions, 1, "the admitted delegate must execute exactly once");
        result.expect("delegate must actually execute")
    }

    // Construct the large observer state in a separate synchronous frame.
    // The enclosing async function still defers all work until first poll.
    #[inline(never)]
    fn boxed_delegate_observation<F, Fut>(make_work: F) -> std::pin::Pin<Box<Fut>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = (Option<ToolResult>, usize, Vec<ChatMessage>)>,
    {
        Box::pin(make_work())
    }

    async fn observe_delegate_dispatch(
        config: crate::config::Config,
        delegate: DelegateTool,
        arguments: serde_json::Value,
        channel: Option<crate::channels::traits::ChannelMessage>,
    ) -> (Option<ToolResult>, usize, Vec<ChatMessage>) {
        boxed_delegate_observation(move || async move {
            let workspace = config.workspace_dir.clone();
            let memory = delegate.memory.clone();
            let parent_tools = delegate.parent_tools.as_ref().clone();
            let captured = Arc::new(parking_lot::Mutex::new(None));
            let executions = Arc::new(AtomicUsize::new(0));
            let mut tools: Vec<Box<dyn Tool>> = parent_tools
                .into_iter()
                .filter(|tool| tool.name() != "delegate")
                .map(|tool| Box::new(ToolArcRef::new(tool)) as Box<dyn Tool>)
                .collect();
            tools.push(Box::new(CapturedDelegate {
                inner: delegate,
                result: Arc::clone(&captured),
                executions: Arc::clone(&executions),
            }));
            let registry = crate::agent::test_admission::registry_with_search(tools);
            let input = format!("delegate {}", arguments["prompt"].as_str().unwrap_or_default());
            let parent_provider = DelegateParentProvider { arguments };
            let configured_endpoint = CONFIGURED_DELEGATE_ENDPOINT.try_with(Clone::clone).ok();
            let admitted = if let Some(endpoint) = configured_endpoint.as_deref() {
                assert!(
                    channel.is_none() && memory.is_none(),
                    "configured alias owns its CLI SQLite source"
                );
                crate::agent::test_admission::AdmittedTurn::configured_delegation(
                    config,
                    &input,
                    Arc::clone(&registry),
                    endpoint,
                    &parent_provider,
                )
                .await
            } else {
                match (channel, memory) {
                    (Some(mut message), Some(memory)) => {
                        message.content = input.clone();
                        crate::agent::test_admission::AdmittedTurn::with_channel_memory(
                            config,
                            message,
                            Arc::clone(&registry),
                            memory,
                        )
                        .await
                    }
                    (Some(_), None) => panic!("channel delegate fixture requires its observed memory"),
                    (None, Some(memory)) => {
                        crate::agent::test_admission::AdmittedTurn::with_memory(
                            config,
                            &input,
                            Arc::clone(&registry),
                            memory,
                        )
                        .await
                    }
                    (None, None) => {
                        crate::agent::test_admission::AdmittedTurn::with_config(config, &input, Arc::clone(&registry))
                            .await
                    }
                }
            };
            #[cfg(unix)]
            if super::delegate_diagnostic::active() {
                super::delegate_diagnostic::admission(admitted.decision().context, parent_provider.capabilities());
            }
            let mut history = vec![crate::providers::ChatMessage::user(input)];
            let hooks = HookManager::new(workspace);
            let multimodal = crate::config::MultimodalConfig::default();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                crate::agent::turn_engine::run_turn(
                    &mut history,
                    crate::agent::turn_request::TurnRequest {
                        channel_name: "agent",
                        scope: None,
                        mode: Default::default(),
                        silent: true,
                        expose_stay_silent: false,
                        multimodal: &multimodal,
                        schedule: crate::agent::turn_request::TurnSchedule {
                            read_only_tool_concurrency_window: 2,
                            priority_scheduling_enabled: false,
                            low_priority_tool_names: Vec::new(),
                        },
                        compaction: Some(admitted.compaction()),
                        decision: admitted.decision(),
                    },
                    crate::agent::turn_request::TurnServices {
                        provider: &parent_provider,
                        provider_name: "ollama",
                        model: "prx-local-model",
                        temperature: 0.0,
                        tools_registry: registry,
                        observer: &NoopObserver,
                        hooks: &hooks,
                        approval: None,
                        approval_resolver: None,
                        runtime_adapter: Some(admitted.adapter()),
                        memory: admitted.memory_runtime(),
                    },
                    crate::agent::turn_request::TurnDelivery {
                        cancellation: None,
                        on_delta: None,
                        on_tool_call: None,
                    },
                ),
            )
            .await;
            #[cfg(unix)]
            if super::delegate_diagnostic::active() {
                let kind = match &result {
                    Err(_) => "timeout",
                    Ok(Err(_)) => "engine_error",
                    Ok(Ok(_)) => "engine_success",
                };
                super::delegate_diagnostic::outcome(kind, executions.load(Ordering::SeqCst), captured.lock().is_some());
            }
            if configured_endpoint.is_some() {
                let _ = admitted.decision().context.terminal_execution_trace();
            }
            result
                .expect("bounded local delegate fixture")
                .expect("parent execution");
            let result = captured.lock().take();
            (result, executions.load(Ordering::SeqCst), history)
        })
        .await
    }

    #[tokio::test]
    async fn agentic_mode_accepts_and_executes_one_explicit_dynamic_alias() {
        let work = || {
            run_configured_delegate_fixture(
                vec!["dynamic_alias".to_owned()],
                vec![Arc::new(DynamicAliasTool)],
                "dynamic_alias",
                json!({}),
                None,
                false,
                None,
            )
        };
        #[cfg(unix)]
        let (result, requests) = super::delegate_diagnostic::scope(work)
            .await
            .expect("complete private delegate diagnostic");
        #[cfg(not(unix))]
        let (result, requests) = work().await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.ends_with("\ndone"));
        assert_eq!(requests.len(), 2, "child must call its tool and finish");
        assert!(
            requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool" && message["content"] == "dynamic_alias"),
            "the alias must execute, not merely be offered"
        );
    }

    /// INT-2B USERINFO-3, RED before: the agentic path logged the child's
    /// provider label raw in its compaction-window debug line. After: a
    /// `custom:` URL's `user:password@` is redacted.
    #[tokio::test]
    async fn agentic_mode_never_logs_provider_url_userinfo() {
        // Log-capturing tests share per-callsite interest process-wide.
        let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
        #[derive(Clone, Default)]
        struct Sink(Arc<parking_lot::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer({
                let sink = sink.clone();
                move || sink.clone()
            })
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();

        let (result, requests) = run_admitted_delegate_fixture(
            vec!["dynamic_alias".to_owned()],
            vec![Arc::new(DynamicAliasTool)],
            "dynamic_alias",
            json!({}),
            None,
            true,
            None,
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.ends_with("\ndone"));
        assert_eq!(requests.len(), 2, "custom provider must execute and finish");
        assert!(
            requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool" && message["content"] == "dynamic_alias")
        );
        let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
        assert!(
            logs.contains("delegate resolved child compaction window"),
            "the sensitive logging callsite must actually execute: {logs}"
        );
        assert!(logs.contains("custom:http://[redacted]@127.0.0.1:"), "{logs}");
        assert!(!logs.contains("DELEGATE-SECRET-pw"), "{logs}");
    }

    #[tokio::test]
    async fn agentic_mode_inherits_parent_tools_only_with_explicit_wildcard() {
        let executions = Arc::new(AtomicUsize::new(0));
        let (result, requests) = run_admitted_delegate_fixture(
            vec!["*".to_owned()],
            vec![
                Arc::new(EchoTool {
                    executions: Arc::clone(&executions),
                }),
                Arc::new(DelegateTool::new(HashMap::new(), test_security())),
            ],
            "echo_tool",
            json!({"value":"fixture"}),
            None,
            false,
            None,
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("(ollama/prx-local-model, agentic)"));
        assert!(result.output.ends_with("\ndone"));
        assert_eq!(requests.len(), 2);
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "the delegated tool must actually run, not be refused"
        );
    }

    #[tokio::test]
    async fn agentic_mode_rejects_wildcard_mixed_with_named_tools() {
        let config = agentic_config(vec!["*".to_string(), "echo_tool".to_string()]);
        let tool = DelegateTool::new(HashMap::new(), test_security())
            .with_parent_tools(Arc::new(vec![Arc::new(EchoTool::default())]));
        let provider = OneToolThenFinalProvider;

        let result = tool
            .execute_tool_loop(
                "agentic",
                &config,
                "openrouter",
                "model-test",
                &provider,
                "run",
                0.2,
                None,
                None,
            )
            .await
            .unwrap();

        assert!(!result.result.success);
        assert!(
            result
                .result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("must use allowed_tools=['*'] exclusively")
        );
    }

    #[tokio::test]
    async fn execute_agentic_runs_tool_call_loop_with_filtered_tools() {
        let executions = Arc::new(AtomicUsize::new(0));
        let (result, requests) = run_admitted_delegate_fixture(
            vec!["echo_tool".to_owned()],
            vec![
                Arc::new(EchoTool {
                    executions: Arc::clone(&executions),
                }),
                Arc::new(DelegateTool::new(HashMap::new(), test_security())),
            ],
            "echo_tool",
            json!({"value":"fixture"}),
            None,
            false,
            None,
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("(ollama/prx-local-model, agentic)"));
        assert!(result.output.ends_with("\ndone"));
        assert_eq!(requests.len(), 2);
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "the delegated tool must actually run, not be refused"
        );
    }

    #[test]
    fn delegate_inline_model_override_drives_child_compaction_resolution() {
        let tool = DelegateTool::new(single_agent("tester", "openrouter", "config-model"), test_security())
            .with_compaction_resolver(compaction_resolver_with_models(vec![
                router_model("openrouter", "config-model", 1_000_000),
                router_model("openrouter", "inline-small", 200_000),
            ]));

        let resolved = tool.resolve_child_compaction("openrouter", "inline-small");

        assert_eq!(resolved.config.max_context_tokens, 200_000);
        assert_eq!(resolved.selected_model, "inline-small");
        assert_ne!(resolved.config.max_context_tokens, 1_000_000);
    }

    #[tokio::test]
    async fn execute_agentic_rejects_delegate_as_ineligible() {
        let config = agentic_config(vec!["delegate".to_string()]);
        let tool = DelegateTool::new(HashMap::new(), test_security()).with_parent_tools(Arc::new(vec![Arc::new(
            DelegateTool::new(HashMap::new(), test_security()),
        )]));

        let provider = OneToolThenFinalProvider;
        let result = tool
            .execute_tool_loop(
                "agentic",
                &config,
                "openrouter",
                "model-test",
                &provider,
                "run",
                0.2,
                None,
                None,
            )
            .await
            .unwrap();

        assert!(!result.result.success);
        assert!(
            result
                .result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("unknown or ineligible tools: delegate")
        );
    }

    #[tokio::test]
    async fn execute_agentic_propagates_provider_errors() {
        let executions = Arc::new(AtomicUsize::new(0));
        let (result, requests) = run_admitted_delegate_fixture(
            vec!["echo_tool".to_owned()],
            vec![Arc::new(EchoTool {
                executions: Arc::clone(&executions),
            })],
            "echo_tool",
            json!({"value":"fixture"}),
            Some("provider boom"),
            false,
            None,
        )
        .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("provider boom"));
        assert_eq!(requests.len(), 1, "the admitted child must reach the failing provider");
        assert_eq!(
            executions.load(Ordering::SeqCst),
            0,
            "a provider failure must not execute the child tool"
        );
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
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );
        agents
    }

    #[test]
    fn delegate_fallback_credential_is_scoped_to_its_provider() {
        let mut child = single_agent("tester", "kimi-code", "k3")
            .remove("tester")
            .expect("test agent");
        let tool = DelegateTool::new(HashMap::new(), test_security()).with_root_connection(RootConnection {
            provider: Some("custom:http://127.0.0.1:18083/v1".to_string()),
            key: Some("root-key".to_string()),
            api_url: None,
        });

        assert_eq!(tool.child_connection(&child, "kimi-code").api_key, None);
        assert_eq!(
            tool.child_connection(&child, "CUSTOM:http://127.0.0.1:18083/v1")
                .api_key,
            Some("root-key")
        );

        child.api_key = Some("child-key".to_string());
        assert_eq!(tool.child_connection(&child, "kimi-code").api_key, Some("child-key"));
    }

    #[test]
    fn schema_exposes_inline_model_and_provider() {
        let tool = DelegateTool::new(sample_agents(), test_security());
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
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = false;
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = single_agent("tester", "mock", "config-model");
        let tool = DelegateTool::new(
            config.agents.clone(),
            Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path())),
        );
        let result = dispatch_admitted_delegate(
            config,
            tool,
            json!({"agent":"tester", "prompt":"hello", "model":"pinned-model"}),
        )
        .await;

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
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = false;
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = single_agent("tester", "mock", "m");
        let tool = DelegateTool::new(
            config.agents.clone(),
            Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path())),
        );
        let result = dispatch_admitted_delegate(
            config,
            tool,
            json!({"agent":"tester", "prompt":"hello", "provider":"totally-invalid-provider"}),
        )
        .await;

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
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = false;
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = single_agent("tester", "mock", "config-model");
        let tool = DelegateTool::new(
            config.agents.clone(),
            Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path())),
        );
        let result = dispatch_admitted_delegate(config, tool, json!({"agent":"tester", "prompt":"hello"})).await;

        assert!(result.success, "mock provider should succeed: {:?}", result.error);
        assert!(
            result.output.contains("mock/config-model"),
            "output should reflect agent config provider/model: {}",
            result.output
        );
    }
}

/// C0-C: the model may name a provider per call, but only one the operator
/// configured, and each provider receives only its own credential. Driven
/// through `DelegateTool::execute`, the real provider factory and loopback
/// endpoints; every credential is a sentinel.
#[cfg(test)]
mod credential_containment_tests {
    use super::*;
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
    use serde_json::json;

    const AGENT_SENTINEL: &str = "C0-DELEGATE-AGENT-SENTINEL";
    const ROOT_SENTINEL: &str = "C0-DELEGATE-ROOT-SENTINEL";

    fn agent(provider: &str, api_key: Option<&str>) -> DelegateAgentConfig {
        DelegateAgentConfig {
            provider: provider.to_string(),
            model: PROBE_MODEL.to_string(),
            system_prompt: None,
            api_key: api_key.map(str::to_string),
            credential_ref: None,
            temperature: None,
            max_depth: 3,
            agentic: false,
            allowed_tools: Vec::new(),
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: None,
        }
    }

    fn security(temp: &tempfile::TempDir) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            workspace_dir: temp.path().to_path_buf(),
            ..SecurityPolicy::default()
        })
    }

    fn options(temp: &tempfile::TempDir) -> providers::ProviderRuntimeOptions {
        providers::ProviderRuntimeOptions {
            openprx_dir: Some(temp.path().to_path_buf()),
            codex_auth_json_path: Some(temp.path().join("no-codex-auth.json")),
            codex_auth_json_auto_import: false,
            ..providers::ProviderRuntimeOptions::default()
        }
    }

    fn admitted_config(
        temp: &tempfile::TempDir,
        agents: HashMap<String, DelegateAgentConfig>,
    ) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.workspace_dir = temp.path().to_owned();
        config.config_path = temp.path().join("config.toml");
        config.default_provider = Some("ollama".to_owned());
        config.default_model = Some("prx-local-model".to_owned());
        config.causal_tree.enabled = false;
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        config.reliability.provider_retries = 0;
        config.agents = agents;
        config
    }

    /// PMM-C: a delegation to a model that cannot chat is refused before any
    /// provider exists, whether the agent config or the call names it.
    #[tokio::test]
    async fn a_delegation_to_a_decision_only_model_is_refused_before_transport() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let own = CredentialProbe::start().await;
        let mut decision = agent(&own.custom_provider(), Some(AGENT_SENTINEL));
        decision.model = "jev-decision-test".to_string();
        let agents = HashMap::from([("a".to_string(), decision)]);
        let mut config = admitted_config(&temp, agents.clone());
        config
            .providers
            .models
            .push(crate::config::schema::ProviderModelConfig {
                provider: own.custom_provider(),
                model: Some("jev-decision-test".to_owned()),
                operations: Some(vec![crate::config::schema::ModelOperationSetting::DecisionEndpoint]),
                native_tools: None,
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: None,
                tool_call_dialect: None,
                context_window: None,
            });
        let tool = DelegateTool::new_with_options(agents, security(&temp), options(&temp));
        let result =
            super::tests::dispatch_admitted_delegate(config, tool, json!({"agent": "a", "prompt": "hi"})).await;
        assert!(!result.success, "a decision-only model served a delegation");
        let error = result.error.unwrap_or_default();
        assert!(error.contains("cannot be used for chat"), "{error}");
        assert!(own.requests().is_empty(), "nothing is sent");
        assert!(!error.contains(AGENT_SENTINEL));
    }

    /// A `custom:<url>` the operator never configured is refused, and the
    /// named agent's key never reaches it.
    #[tokio::test]
    async fn a_model_named_endpoint_the_operator_did_not_configure_is_refused() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let own = CredentialProbe::start().await;
        let foreign = CredentialProbe::start().await;
        let agents = HashMap::from([("a".to_string(), agent(&own.custom_provider(), Some(AGENT_SENTINEL)))]);
        let config = admitted_config(&temp, agents.clone());
        let tool = DelegateTool::new_with_options(agents, security(&temp), options(&temp));
        let result = super::tests::dispatch_admitted_delegate(
            config,
            tool,
            json!({"agent": "a", "prompt": "hi", "provider": foreign.custom_provider()}),
        )
        .await;

        assert!(!result.success, "an unconfigured endpoint must be refused");
        let error = result.error.unwrap_or_default();
        assert!(error.contains("not configured"), "{error}");
        assert!(
            foreign.requests().is_empty(),
            "nothing may reach an unconfigured endpoint"
        );
        assert!(!foreign.saw(AGENT_SENTINEL));
        assert!(own.requests().is_empty());
    }

    /// A configured provider named per call gets only its own credential: the
    /// named agent's key stays with the agent's own provider.
    #[tokio::test]
    async fn a_named_agent_key_stays_with_its_own_provider() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let own = CredentialProbe::start().await;
        let other = CredentialProbe::start().await;
        let agents = HashMap::from([
            ("a".to_string(), agent(&own.custom_provider(), Some(AGENT_SENTINEL))),
            ("b".to_string(), agent(&other.custom_provider(), None)),
        ]);
        let config = admitted_config(&temp, agents.clone());
        let tool = DelegateTool::new_with_options(agents, security(&temp), options(&temp));
        let result = super::tests::dispatch_admitted_delegate(
            config,
            tool,
            json!({"agent": "a", "prompt": "hi", "provider": other.custom_provider()}),
        )
        .await;

        assert!(
            result.success,
            "a configured keyless endpoint serves: {:?}",
            result.error
        );
        let requests = other.requests();
        assert_eq!(requests.len(), 1);
        assert!(!other.saw(AGENT_SENTINEL), "agent a's key reached another provider");
        assert!(requests.iter().all(|request| request.authorization().is_none()));
    }

    #[tokio::test]
    async fn delegated_hint_uses_only_its_selected_route_key_on_wire() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let root = CredentialProbe::start().await;
        let route = CredentialProbe::start().await;
        let mut named = agent(&root.custom_provider(), Some("PRX-DELEGATE-ROOT-KEY"));
        named.model = "hint:fast".to_string();
        let agents = HashMap::from([("prx_agent".to_string(), named)]);
        let mut config = admitted_config(&temp, agents.clone());
        config.default_provider = Some(root.custom_provider());
        config.api_key = Some("PRX-DELEGATE-ROOT-KEY".to_string());
        config.agents = agents.clone();
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "fast".to_string(),
            provider: route.custom_provider(),
            model: PROBE_MODEL.to_string(),
            api_key: Some("PRX-DELEGATE-ROUTE-KEY".to_string()),
            credential_ref: None,
        });
        let tool = DelegateTool::new_with_options(agents, security(&temp), options(&temp))
            .with_hint_config(&config)
            .with_model_catalog(providers::selection::catalog_for(&config))
            .with_configured_providers(&providers::ConfiguredProviders::from_config(&config));
        let result =
            super::tests::dispatch_admitted_delegate(config, tool, json!({"agent": "prx_agent", "prompt": "probe"}))
                .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(route.wait_for_requests(1, std::time::Duration::from_secs(10)).await, 1);
        assert!(route.saw("PRX-DELEGATE-ROUTE-KEY"));
        assert!(!route.saw("PRX-DELEGATE-ROOT-KEY"));
        assert!(root.requests().is_empty());
    }

    /// Guard: without an override the named agent presents its own key, and
    /// the root key goes only to the provider it was configured for.
    #[tokio::test]
    async fn own_credentials_still_reach_their_own_providers() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let own = CredentialProbe::start().await;
        let root = CredentialProbe::start().await;
        let agents = HashMap::from([
            ("a".to_string(), agent(&own.custom_provider(), Some(AGENT_SENTINEL))),
            ("b".to_string(), agent(&root.custom_provider(), None)),
        ]);
        let mut config = admitted_config(&temp, agents.clone());
        config.default_provider = Some(root.custom_provider());
        config.api_key = Some(ROOT_SENTINEL.to_owned());
        let first_tool = DelegateTool::new_with_options(agents.clone(), security(&temp), options(&temp));
        let second_tool = DelegateTool::new_with_options(agents, security(&temp), options(&temp));
        let first = super::tests::dispatch_admitted_delegate(
            config.clone(),
            first_tool,
            json!({"agent": "a", "prompt": "first credential probe"}),
        )
        .await;
        let second = super::tests::dispatch_admitted_delegate(
            config,
            second_tool,
            json!({"agent": "b", "prompt": "second credential probe"}),
        )
        .await;

        assert!(first.success && second.success, "{:?} {:?}", first.error, second.error);
        assert!(own.saw(AGENT_SENTINEL) && !own.saw(ROOT_SENTINEL));
        assert!(root.saw(ROOT_SENTINEL) && !root.saw(AGENT_SENTINEL));
    }

    // ── C0-f F-1: the root key travels only with the root URL ─────────────

    const F1_ROOT_SENTINEL: &str = "C0F-DELEGATE-ROOT-SENTINEL";

    fn root_bound_tool(temp: &tempfile::TempDir, default_provider: &str, api_url: &str) -> DelegateTool {
        let agents = HashMap::from([("a".to_string(), agent("groq", None))]);
        DelegateTool::new_with_options(agents, security(temp), options(temp)).with_root_connection(RootConnection {
            provider: Some(default_provider.to_string()),
            key: Some(F1_ROOT_SENTINEL.to_string()),
            api_url: Some(api_url.to_string()),
        })
    }

    /// Reviewer scenario: the root key of a `litellm` default configured with
    /// its own base URL. A model-chosen spelling of the same provider must get
    /// the key and the URL together, never the key on the catalog default host.
    #[test]
    fn a_respelled_default_gets_the_root_key_only_with_the_root_url() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let tool = root_bound_tool(&temp, "litellm", "http://127.0.0.1:9/litellm");
        let child = agent("groq", None);
        assert_eq!(
            providers::canonical_provider_key("lite-llm"),
            providers::canonical_provider_key("litellm")
        );

        for spelling in ["litellm", "lite-llm", "LiteLLM"] {
            let connection = tool.child_connection(&child, spelling);
            assert_eq!(connection.api_key, Some(F1_ROOT_SENTINEL), "{spelling}");
            assert_eq!(connection.api_url, Some("http://127.0.0.1:9/litellm"), "{spelling}");
            assert!(connection.released, "{spelling}");
        }
    }

    /// INT-2B a33 (R-C0f N-5, D-030), RED before: the root connection was
    /// released to a child only when the root had a key, so a keyless
    /// default (a remote Ollama given by `api_url`) lost its base URL and the
    /// child went to the catalog default host, while the worker path kept
    /// it. After: the root URL is released to the same provider identity
    /// with or without a key; another provider still gets nothing.
    #[test]
    fn a_keyless_root_default_keeps_its_base_url() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let tool = DelegateTool::new_with_options(HashMap::new(), security(&temp), options(&temp))
            .with_root_connection(RootConnection {
                provider: Some("ollama".to_string()),
                key: None,
                api_url: Some("http://127.0.0.1:9/ollama".to_string()),
            });
        let child = agent("ollama", None);

        let connection = tool.child_connection(&child, "ollama");
        assert_eq!(connection.api_url, Some("http://127.0.0.1:9/ollama"));
        assert_eq!(connection.api_key, None);
        assert!(connection.released);

        let groq = agent("groq", None);
        let other = tool.child_connection(&groq, "groq");
        assert_eq!(other.api_url, None);
        assert!(!other.released);
    }

    /// B4f (R-INT2BB4 report L-9): pins a33's accepted rotation-key change.
    /// The root rotation keys (`reliability.api_keys`) follow the keyless
    /// root connection to a child on the same provider identity, and a child
    /// on another identity gets none. Checked on the scoped settings and on
    /// the wire: the root endpoint answers the keyless first request `429`,
    /// so the chain rotates to the rotation key, which that endpoint then
    /// sees; the other endpoint never sees it.
    #[tokio::test]
    async fn a_keyless_root_releases_its_rotation_keys_only_to_its_own_identity() {
        const ROTATION_SENTINEL: &str = "B4F-ROTATION-KEY-SENTINEL";
        let temp = tempfile::tempdir().expect("test: temp dir");
        let root = CredentialProbe::start_with_statuses(vec![429]).await;
        let other = CredentialProbe::start_with_statuses(vec![429]).await;
        let agents = HashMap::from([
            ("same".to_string(), agent(&root.custom_provider(), None)),
            ("other".to_string(), agent(&other.custom_provider(), None)),
        ]);
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 0,
            provider_backoff_ms: 0,
            api_keys: vec![ROTATION_SENTINEL.to_string()],
            ..crate::config::ReliabilityConfig::default()
        };
        let mut config = admitted_config(&temp, agents.clone());
        config.default_provider = Some(root.custom_provider());
        config.api_key = None;
        config.reliability = reliability.clone();
        let other_tool = DelegateTool::new_with_options(agents.clone(), security(&temp), options(&temp));
        let tool = DelegateTool::new_with_options(agents, security(&temp), options(&temp))
            .with_root_connection(RootConnection {
                provider: Some(root.custom_provider()),
                key: None,
                api_url: None,
            })
            .with_reliability(reliability);

        // The scoped settings the child chain is built with.
        let same_child = agent(&root.custom_provider(), None);
        let same = tool.child_connection(&same_child, &root.custom_provider());
        assert!(same.released);
        assert_eq!(
            providers::scoped_reliability(&tool.reliability, &same).api_keys,
            vec![ROTATION_SENTINEL.to_string()]
        );
        let other_child = agent(&other.custom_provider(), None);
        let different = tool.child_connection(&other_child, &other.custom_provider());
        assert!(!different.released);
        assert!(
            providers::scoped_reliability(&tool.reliability, &different)
                .api_keys
                .is_empty()
        );

        // On the wire.
        let served = super::tests::dispatch_admitted_delegate(
            config.clone(),
            tool,
            json!({"agent": "same", "prompt": "root rotation probe"}),
        )
        .await;
        assert!(served.success, "{:?}", served.error);
        assert!(root.saw(ROTATION_SENTINEL), "the rotation key reached its own identity");
        let _ = super::tests::dispatch_admitted_delegate(
            config,
            other_tool,
            json!({"agent": "other", "prompt": "foreign rotation probe"}),
        )
        .await;
        assert!(!other.requests().is_empty(), "the other endpoint was reached");
        assert!(
            !other.saw(ROTATION_SENTINEL),
            "another identity never gets the rotation key"
        );
    }

    /// The `openai` + corporate gateway variant.
    #[test]
    fn a_respelled_openai_default_keeps_its_corporate_url() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let tool = root_bound_tool(&temp, "openai", "https://gateway.corp.example/v1");
        let child = agent("groq", None);

        for spelling in ["openai", "OpenAI"] {
            let connection = tool.child_connection(&child, spelling);
            assert_eq!(connection.api_key, Some(F1_ROOT_SENTINEL), "{spelling}");
            assert_eq!(
                connection.api_url,
                Some("https://gateway.corp.example/v1"),
                "{spelling}"
            );
        }
    }

    /// A named agent's own key is its own credential, with no root URL and no
    /// root rotation keys; another provider gets nothing.
    #[test]
    fn an_agent_key_never_borrows_the_root_url() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let tool = root_bound_tool(&temp, "litellm", "http://127.0.0.1:9/litellm");
        let child = agent("litellm", Some(AGENT_SENTINEL));

        let own = tool.child_connection(&child, "lite-llm");
        assert_eq!(own.api_key, Some(AGENT_SENTINEL));
        assert_eq!(own.api_url, None);
        assert!(!own.released);

        let groq = agent("groq", None);
        let other = tool.child_connection(&groq, "mistral");
        assert_eq!((other.api_key, other.api_url, other.released), (None, None, false));
    }

    /// On the wire: a respelled default reaches the root URL with the root key.
    #[tokio::test]
    async fn f1_wire_a_respelled_default_reaches_only_the_root_url() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let root = CredentialProbe::start().await;
        let tool = root_bound_tool(&temp, "litellm", &root.base_url());

        let mut config = admitted_config(&temp, tool.agents.as_ref().clone());
        config.default_provider = Some("litellm".to_owned());
        config.api_key = Some(F1_ROOT_SENTINEL.to_owned());
        config.api_url = Some(root.base_url());
        let result = super::tests::dispatch_admitted_delegate(
            config,
            tool,
            json!({"agent": "a", "prompt": "hi", "provider": "lite-llm"}),
        )
        .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(root.requests().len(), 1);
        assert!(root.saw(F1_ROOT_SENTINEL));
    }

    /// USERINFO-2: the delegate tool names its provider in the output the
    /// parent model reads, in its failure text and in its durable events. A
    /// `custom:` URL may carry `user:password@`; none of them may carry it,
    /// while the request still reaches the endpoint.
    #[tokio::test]
    async fn delegate_names_its_provider_without_url_userinfo() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let probe = CredentialProbe::start().await;
        let label = probe
            .custom_provider()
            .replacen("http://", "http://u:SECRET-fixture-pw@", 1);
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
        let agents = HashMap::from([
            ("a".to_string(), agent(&label, Some(AGENT_SENTINEL))),
            (
                "broken".to_string(),
                agent(
                    "custom:http://u:SECRET-fixture-pw@127.0.0.1:99999/v1",
                    Some(AGENT_SENTINEL),
                ),
            ),
        ]);
        let config = admitted_config(&temp, agents.clone());
        let success_tool = DelegateTool::new_with_options(agents.clone(), security(&temp), options(&temp))
            .with_shared_memory(Arc::clone(&memory));
        let failure_tool = DelegateTool::new_with_options(agents, security(&temp), options(&temp))
            .with_shared_memory(Arc::clone(&memory));
        let served = super::tests::dispatch_admitted_delegate(
            config.clone(),
            success_tool,
            json!({"agent": "a", "prompt": "provider label success"}),
        )
        .await;
        assert!(served.success, "{served:?}");
        assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
        assert!(served.output.contains("[redacted]@127.0.0.1"), "{}", served.output);
        assert!(!served.output.contains("SECRET-fixture-pw"), "{}", served.output);

        let failed = super::tests::dispatch_admitted_delegate(
            config,
            failure_tool,
            json!({"agent": "broken", "prompt": "provider label failure"}),
        )
        .await;
        assert!(!failed.success, "invalid provider must fail");
        let error = failed.error.unwrap_or_default();
        assert!(error.contains("Failed to create provider"), "{error}");
        assert!(!error.contains("SECRET-fixture-pw"), "{error}");

        let events = memory
            .list_message_events_recent(
                &crate::memory::MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: temp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: None,
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                100,
            )
            .await
            .expect("test: events readable");
        assert!(
            events.iter().any(|event| event.event_type == "turn.finalized"),
            "the successful delegate must persist its terminal event"
        );
        let stored = serde_json::to_string(&events).expect("test: events serialize");
        assert!(
            !stored.contains("SECRET-fixture-pw"),
            "a durable event kept the password: {stored}"
        );
    }

    // ── D-053 / REDACT-TURN persistence probes ────────────────────────────

    /// Every event the delegate recorded under the temp workspace.
    async fn workspace_events(memory: &Arc<dyn Memory>, temp: &tempfile::TempDir) -> Vec<crate::memory::MessageEvent> {
        memory
            .list_message_events_recent(
                &crate::memory::MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: temp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: None,
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                100,
            )
            .await
            .expect("test: events readable")
    }

    /// D-053 / REDACT-TURN probe: a non-agentic delegation whose provider reply
    /// carries a secret commits through `finalize_delegate_turn` (success
    /// branch) and the result/task records; nothing under the workspace may
    /// keep the secret. It must fail before the REDACT-TURN commits.
    #[tokio::test]
    async fn delegate_persists_no_secret_from_a_reply() {
        use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};

        let temp = tempfile::tempdir().expect("test: temp dir");
        let probe = CredentialProbe::start_replying(&format!("Here is the key {PROBE_SECRET} you asked for")).await;
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
        let agents = HashMap::from([("a".to_string(), agent(&probe.custom_provider(), None))]);
        let config = admitted_config(&temp, agents.clone());
        let tool = DelegateTool::new_with_options(agents, security(&temp), options(&temp))
            .with_shared_memory(Arc::clone(&memory));
        let served =
            super::tests::dispatch_admitted_delegate(config, tool, json!({"agent": "a", "prompt": "hi"})).await;
        assert!(served.success, "{served:?}");
        assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
        assert!(
            served.output.contains(PROBE_SECRET),
            "the reply must carry the secret for the probe to mean anything: {}",
            served.output
        );
        let events = workspace_events(&memory, &temp).await;
        assert!(
            events.iter().any(|event| event.event_type == "turn.finalized"),
            "the delegate must reach the terminal commit: {events:?}"
        );

        // The workspace, the SQLite memory and the provider runtime dir are
        // all this temp dir.
        assert_nothing_under_persists_the_probe_secret(temp.path());
    }

    /// D-053 / REDACT-TURN probe: a non-agentic delegation whose provider fails
    /// with an error carrying a secret commits through `finalize_delegate_turn`
    /// (failure branch) and the result/task records; nothing under the
    /// workspace may keep the secret. It must fail before the REDACT-TURN commits.
    #[tokio::test]
    async fn delegate_persists_no_secret_from_a_provider_error() {
        use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};

        let temp = tempfile::tempdir().expect("test: temp dir");
        let probe = CredentialProbe::start_failing(400, &format!("upstream rejected key {PROBE_SECRET}")).await;
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
        let agents = HashMap::from([("a".to_string(), agent(&probe.custom_provider(), None))]);
        let config = admitted_config(&temp, agents.clone());
        let tool = DelegateTool::new_with_options(agents, security(&temp), options(&temp))
            .with_shared_memory(Arc::clone(&memory));
        let failed =
            super::tests::dispatch_admitted_delegate(config, tool, json!({"agent": "a", "prompt": "hi"})).await;
        assert!(!failed.success, "a failing upstream must fail the delegation");
        assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
        let error = failed.error.unwrap_or_default();
        // The upstream's error text must reach the turn: verbatim before the
        // redactors merged (D-054), already redacted by the provider layer since.
        assert!(
            error.contains(PROBE_SECRET) || error.contains("upstream rejected key [REDACTED]"),
            "the provider error must reach the turn for the probe to mean anything: {error}"
        );
        let events = workspace_events(&memory, &temp).await;
        assert!(
            events.iter().any(|event| event.event_type == "turn.finalized"),
            "the delegate must reach the terminal commit: {events:?}"
        );

        // The workspace, the SQLite memory and the provider runtime dir are
        // all this temp dir.
        assert_nothing_under_persists_the_probe_secret(temp.path());
    }
}
