#![cfg(test)]

//! Engine fixtures obtain decisions through the actual CLI admission path.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::agent::turn_request::TurnDecision;
use crate::causal_tree::state::TurnSurfaceKind;
use crate::decision::factory::{BackendSpec, DecisionTransports};
use crate::decision::telemetry::{CausalEvent, CausalEventSink};
use crate::runtime::turn_decision::{
    AgentTurnKind, TurnDecisionContext, TurnDecisionService, TurnServiceBuild, build_turn_decision_service,
};
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::capability::catalog::{CatalogSources, SurfacePolicy};
use crate::tools::execution::{
    AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionService,
    TracingToolExecutionAudit,
};

pub(crate) struct NoExternalTransport;

impl DecisionTransports for NoExternalTransport {
    fn build(
        &self,
        _: &BackendSpec<'_>,
    ) -> Result<Box<dyn crate::decision::backend::DecisionBackend>, crate::decision::DecisionError> {
        Err(crate::decision::DecisionError::Unavailable)
    }
}

pub(crate) struct SilentEvents;

impl CausalEventSink for SilentEvents {
    fn emit(&self, _: CausalEvent) {}
}

/// Match production registry wiring before sharing it with service and engine.
/// Registration supplies a backend, not permission to publish or execute it.
pub(crate) fn registry_with_search(
    mut tools: Vec<Box<dyn crate::tools::Tool>>,
) -> Arc<Vec<Box<dyn crate::tools::Tool>>> {
    if !tools.iter().any(|tool| tool.name() == "capability_search") {
        tools.push(Box::new(crate::tools::capability::search_tool::CapabilitySearchTool));
    }
    Arc::new(tools)
}

enum FixtureOrigin {
    Cli { recorded: bool },
    Channel(Box<crate::channels::traits::ChannelMessage>),
}

pub(crate) struct AdmittedTurn {
    service: Arc<TurnDecisionService>,
    context: Arc<TurnDecisionContext>,
    proof: Box<dyn TurnSourceProof>,
    execution: Arc<ToolExecutionService>,
    execution_context: crate::tools::ToolExecutionContext,
    memory: Arc<dyn crate::memory::Memory>,
    fabric: crate::memory::MemoryFabric,
}

impl AdmittedTurn {
    pub(crate) async fn empty(workspace: &Path, input: &str) -> Self {
        Self::with_tools(workspace, input, Arc::new(Vec::new())).await
    }

    pub(crate) async fn with_tools(
        workspace: &Path,
        input: &str,
        registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    ) -> Self {
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.to_path_buf();
        Self::with_config(config, input, registry).await
    }

    pub(crate) async fn with_config(
        config: crate::config::Config,
        input: &str,
        registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    ) -> Self {
        let memory: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(&config.workspace_dir).expect("fixture memory"));
        Self::build(
            config,
            input,
            registry,
            memory,
            FixtureOrigin::Cli { recorded: false },
            ("ollama", "prx-local-model"),
        )
        .await
    }

    pub(crate) async fn with_memory(
        config: crate::config::Config,
        input: &str,
        registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
        memory: Arc<dyn crate::memory::Memory>,
    ) -> Self {
        Self::with_model(config, input, registry, memory, ("ollama", "prx-local-model")).await
    }

    pub(crate) async fn with_model(
        config: crate::config::Config,
        input: &str,
        registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
        memory: Arc<dyn crate::memory::Memory>,
        selection: (&str, &str),
    ) -> Self {
        Self::build(
            config,
            input,
            registry,
            memory,
            FixtureOrigin::Cli { recorded: true },
            selection,
        )
        .await
    }

    pub(crate) async fn with_channel(
        config: crate::config::Config,
        message: crate::channels::traits::ChannelMessage,
        registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    ) -> Self {
        let memory: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(&config.workspace_dir).expect("channel fixture memory"));
        Self::with_channel_memory(config, message, registry, memory).await
    }

    pub(crate) async fn with_channel_memory(
        config: crate::config::Config,
        message: crate::channels::traits::ChannelMessage,
        registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
        memory: Arc<dyn crate::memory::Memory>,
    ) -> Self {
        let input = message.content.clone();
        Self::build(
            config,
            &input,
            registry,
            memory,
            FixtureOrigin::Channel(Box::new(message)),
            ("ollama", "prx-local-model"),
        )
        .await
    }

    async fn build(
        config: crate::config::Config,
        input: &str,
        registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
        memory: Arc<dyn crate::memory::Memory>,
        origin: FixtureOrigin,
        selection: (&str, &str),
    ) -> Self {
        Self::build_mode(config, input, registry, memory, origin, selection, None).await
    }

    /// Explicit engine-only Answer fixture; existing Off/Builtin helpers are unchanged.
    pub(crate) async fn configured_answer(workspace: &Path, input: &str, endpoint: &str) -> Self {
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.to_owned();
        config.config_path = workspace.join("config.toml");
        crate::agent::agent::native_decision_fixture::configure(&mut config, endpoint)
            .expect("actual configured decision endpoint");
        config.cost.prices.insert(
            "ollama/prx-local-model".to_owned(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        let memory: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace).expect("owned Answer fixture SQLite"));
        Self::build_mode(
            config,
            input,
            Arc::new(Vec::new()),
            memory,
            FixtureOrigin::Cli { recorded: true },
            ("ollama", "prx-local-model"),
            Some(false),
        )
        .await
    }

    /// Explicit real native-schema fixture; legacy disabled helpers are unchanged.
    pub(crate) async fn configured_native_schema(
        mut config: crate::config::Config,
        input: &str,
        registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
        memory: Arc<dyn crate::memory::Memory>,
        endpoint: &str,
        provider: &dyn crate::providers::Provider,
    ) -> Self {
        let capabilities = provider.capabilities();
        assert!(
            capabilities.native_tool_calling,
            "actual fixture provider must consume native schemas"
        );
        assert!(!capabilities.vision, "this fixture declares no vision operation");
        crate::agent::agent::native_decision_fixture::configure(&mut config, endpoint)
            .expect("actual configured native decision endpoint");
        Self::build_mode(
            config,
            input,
            registry,
            memory,
            FixtureOrigin::Cli { recorded: true },
            ("ollama", "prx-local-model"),
            Some(capabilities.native_tool_calling),
        )
        .await
    }

    async fn build_mode(
        mut config: crate::config::Config,
        input: &str,
        registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
        memory: Arc<dyn crate::memory::Memory>,
        origin: FixtureOrigin,
        selection: (&str, &str),
        configured_native_tools: Option<bool>,
    ) -> Self {
        let workspace_path = config.workspace_dir.clone();
        let workspace = workspace_path.as_path();
        if configured_native_tools.is_none() {
            config.causal_tree.enabled = false;
        }
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": selection.0, "model": selection.1,
                "operations": ["chat"], "native_tools": configured_native_tools.unwrap_or(true),
            }))
            .expect("local fixture model descriptor"),
        );
        let channel_surface = matches!(&origin, FixtureOrigin::Channel(_));
        if channel_surface {
            config.tool_tiering = config.tool_tiering.for_channel_surface();
        }
        let generation = crate::config::new_shared(config.clone()).pin();
        let security = Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            workspace,
        ));
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let hooks = crate::hooks::HookDiagnostics {
            configured: false,
            valid: true,
            config_path: String::new(),
            timeout_ms: 0,
            action_count: 0,
            events: BTreeMap::new(),
            catalog_digest: None,
            error: None,
            runs: false,
            blocked: None,
        };
        let provider_catalog = crate::providers::catalog::CatalogView::from_config(&config);
        let service = build_turn_decision_service(
            if channel_surface {
                TurnSurfaceKind::Channel
            } else {
                TurnSurfaceKind::AgentLoop
            },
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution: Arc::clone(&execution),
                catalog_sources: CatalogSources::from_runtime_inputs(
                    generation.id.0.to_string(),
                    &config,
                    Vec::new(),
                    &hooks,
                    0,
                ),
                surface_policy: SurfacePolicy {
                    channel_allowed: channel_surface,
                    excluded_names: config.tool_tiering.always_exclude.iter().cloned().collect(),
                    direct_names: config
                        .tool_tiering
                        .always_include
                        .iter()
                        .filter(|name| !config.tool_tiering.always_exclude.contains(name))
                        .cloned()
                        .collect(),
                    ..Default::default()
                },
                provider_catalog: &provider_catalog,
                transports: if configured_native_tools.is_some() {
                    &crate::decision::compatible_http::HttpDecisionTransports
                } else {
                    &NoExternalTransport
                },
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .expect("actual fixture decision service")
        .with_loop_runtime(crate::runtime::turn_decision::TurnLoopRuntime {
            hooks: Arc::new(crate::hooks::HookManager::new(workspace.to_owned())),
            observer: Arc::new(crate::observability::noop::NoopObserver),
        })
        .expect("fixture loop runtime");
        if configured_native_tools.is_some() {
            crate::runtime::turn_cleanup::current_tracker()
                .expect("Answer fixture must have an actual cleanup owner")
                .retain_service(Arc::clone(&service));
        }
        let (proof, envelope): (Box<dyn TurnSourceProof>, _) = match origin {
            FixtureOrigin::Channel(message) => {
                let (proof, envelope) = crate::channels::decision_source::recorded_fixture_source(
                    &generation,
                    Arc::clone(&memory),
                    &message,
                    "prx_channel_parent",
                    selection,
                )
                .await
                .expect("actual recorded channel fixture source");
                (Box::new(proof), envelope)
            }
            FixtureOrigin::Cli { recorded } => {
                let proof = if recorded {
                    crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                        &generation,
                        Arc::clone(&memory),
                        input,
                        "prx_test_run",
                        selection,
                    )
                    .await
                } else {
                    crate::agent::entry::test_cli_agent_turn_proof_with_input(
                        &generation,
                        selection.0,
                        selection.1,
                        input,
                    )
                };
                let mut envelope =
                    crate::runtime::envelope::RuntimeEnvelope::agent(workspace.to_string_lossy(), proof.run_id())
                        .with_config_generation(&generation);
                if let Some(event_id) = proof.source_event_id() {
                    envelope = envelope.with_source_message_event_id(event_id);
                }
                (Box::new(proof), envelope)
            }
        };
        let context = service
            .decide_agent_initial(
                proof.as_ref(),
                AgentTurnKind::Repl,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("actual fixture turn admission");
        let chat_type = proof.policy_chat_type().to_owned();
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy())
            .with_event_recording(config.memory.event_recording_config());
        let execution_context = crate::tools::ToolExecutionContext::new(envelope, chat_type);
        Self {
            service,
            context,
            proof,
            execution,
            execution_context,
            memory,
            fabric,
        }
    }

    pub(crate) fn adapter(&self) -> crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
        crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
            provider_trace: None,
            events: None,
            stream_provider: false,
            tool_execution_service: Some(Arc::clone(&self.execution)),
            tool_execution_context: self.execution_context.clone(),
            allowed_tool_names: None,
        }
    }

    pub(crate) fn compaction(&self) -> &crate::config::AgentCompactionConfig {
        self.context.main_compaction()
    }

    pub(crate) fn memory_runtime(&self) -> crate::agent::turn_request::ToolLoopMemory {
        crate::agent::turn_request::ToolLoopMemory::new(&self.memory, Path::new(self.proof.workspace_id()), None)
            .with_event_fabric(self.fabric.clone())
            .with_request_event_scope(self.execution_context.envelope.message_scope())
    }

    pub(crate) fn memory_runtime_with_ingest(&self) -> crate::agent::turn_request::ToolLoopMemory {
        let mut runtime = self.memory_runtime();
        runtime.ingest = Some(crate::agent::document_ingest::DocumentIngestRuntime::from_envelope(
            Arc::clone(&self.memory),
            &self.execution_context.envelope,
        ));
        runtime
    }

    pub(crate) fn principal(&self) -> crate::memory::MemoryPrincipal {
        self.execution_context.envelope.memory_principal()
    }

    /// Budget tests must measure the definitions that were actually admitted,
    /// rather than pass because a large registered schema was never selected.
    pub(crate) fn assert_selected_specs(&self, expected: &[crate::tools::ToolSpec]) {
        let selected = self
            .service
            .agent_execution_specs(self.proof.as_ref(), &self.context)
            .expect("admitted fixture tool selection");
        assert_eq!(
            serde_json::to_value(&selected.specs).expect("selected specs"),
            serde_json::to_value(expected).expect("measured specs"),
            "budget fixture must measure the actual admitted tool definitions",
        );
    }

    /// Budget measurements include the separately gated control schema too.
    pub(crate) fn assert_core_specs(&self, expected: &[crate::tools::ToolSpec]) {
        let mut specs = self
            .service
            .agent_execution_specs(self.proof.as_ref(), &self.context)
            .expect("admitted fixture business selection")
            .specs;
        if let Some(search) = self
            .service
            .agent_capability_search_spec(self.proof.as_ref(), &self.context)
            .expect("admitted fixture search control")
        {
            specs.push(search);
        }
        assert_eq!(
            serde_json::to_value(specs).expect("actual core specs"),
            serde_json::to_value(expected).expect("measured core specs"),
            "budget fixture must measure the complete admitted core",
        );
    }

    pub(crate) fn decision(&self) -> TurnDecision<'_> {
        TurnDecision {
            service: &self.service,
            context: &self.context,
            proof: self.proof.as_ref(),
        }
    }
}

#[tokio::test]
async fn channel_admission_retains_recorded_transport_identity() {
    let workspace = tempfile::tempdir().expect("channel fixture workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    let message = crate::channels::traits::ChannelMessage {
        id: "prx-channel-input".to_owned(),
        sender: "prx_user".to_owned(),
        reply_target: "prx_channel".to_owned(),
        content: "review the current result".to_owned(),
        channel: "wacli".to_owned(),
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
    let admitted = AdmittedTurn::with_channel(config, message.clone(), Arc::new(Vec::new())).await;
    let proof = admitted.proof.as_ref();
    assert_eq!(proof.surface(), TurnSurfaceKind::Channel);
    assert_eq!(proof.policy_channel(), "wacli");
    assert_eq!(proof.policy_sender(), "prx_user");
    assert_eq!(proof.routing_input(), message.content);
    let envelope = &admitted.execution_context.envelope;
    assert_eq!(envelope.resolved_owner_id(), proof.owner_id());
    assert_eq!(envelope.session_key, proof.session_key());
    assert_eq!(admitted.execution_context.chat_type, proof.policy_chat_type());
    let event_id = proof.source_event_id().expect("durable channel source id");
    assert_eq!(envelope.source_message_event_id.as_deref(), Some(event_id));
    let recorded = admitted
        .memory
        .find_owned_message_event(proof.workspace_id(), proof.owner_id(), event_id)
        .await
        .expect("channel source readback")
        .expect("persisted channel source");
    assert_eq!(recorded.content, message.content);
    assert_eq!(recorded.channel.as_deref(), Some("wacli"));
    assert_eq!(recorded.sender.as_deref(), Some("prx_user"));
    assert_eq!(recorded.run_id.as_deref(), Some(proof.run_id()));
}
