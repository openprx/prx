#![cfg(test)]
//! Actual parent issuance, child ledger, terminal MAC and successor observations.
//! Controlled decision answers select candidates; no grant or observed fact is forged.
use super::*;
use crate::runtime::child_source::{ChildProviderBinding, ChildSegmentSequence};
use crate::runtime::child_turn::ChildSegmentTurn;
use crate::runtime::turn_cleanup::CleanupTracker;
use crate::runtime::turn_terminal::TerminalUsageCollector;
use crate::tools::{ToolExecutionStatus, ToolSpec};

struct CountedRead {
    inner: crate::tools::file_read::FileReadTool,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for CountedRead {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn tier(&self) -> crate::tools::traits::ToolTier {
        self.inner.tier()
    }
    fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
        self.inner.categories()
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(args).await
    }
}

const HIDDEN_CHILD_CAPABILITY: &str = "prx_hidden_child_read";

struct HiddenChildCapability {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for HiddenChildCapability {
    fn name(&self) -> &str {
        HIDDEN_CHILD_CAPABILITY
    }

    fn description(&self) -> &str {
        "Read hidden child fixture data"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: "hidden".to_owned(),
            error: None,
        })
    }
}

struct ReadBackend {
    identity: BackendIdentity,
}
#[async_trait::async_trait]
impl DecisionBackend for ReadBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }
    fn locality(&self) -> BackendLocality {
        BackendLocality::Builtin
    }
    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }
    // This cfg(test) backend computes controlled answers locally and never
    // performs physical I/O. Explicitly accept the new injected accounting
    // interface without inventing a reservation, receipt, or usage settlement.
    async fn evaluate_with_accounting(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        _accounting: Option<&dyn crate::decision::budget::DecisionAttemptAccounting>,
    ) -> Result<DecisionResponse, DecisionError> {
        self.evaluate(request, body, deadline).await
    }

    async fn evaluate(
        &self,
        request: &DecisionRequest,
        _: &GuardedBody,
        _: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        // Only model answers change. The factory still binds the real compiled
        // request, validates every answer, and runs the ordinary hard gates.
        let mut response = BuiltinDecisionBackend::answer(request)?;
        let has_tool_candidate = crate::decision::builtin::features::extract(request.state().as_json())
            .candidates
            .iter()
            .any(|candidate| candidate.action == Some(crate::decision::builtin::features::Action::UseTool));
        response.backend = "mock".into();
        response.resolved_model = self.identity.configured_model.to_string();
        for (id, answer) in &mut response.answers {
            let work = if has_tool_candidate {
                id.as_str().starts_with("candidate.use_tool-filesystem-")
            } else {
                id.as_str().starts_with("candidate.answer-none.")
            };
            match answer {
                DecisionAnswer::Choice(choice) => {
                    let code = if id == &QuestionId::global(GlobalQuestion::NextAction) {
                        if has_tool_candidate { "use_tool" } else { "answer" }
                    } else {
                        "filesystem"
                    };
                    if let Some(chosen) = choice.probabilities.keys().find(|key| key.as_str() == code).cloned() {
                        for (key, probability) in &mut choice.probabilities {
                            *probability = Probability::new(if key == &chosen { 1.0 } else { 0.0 })?;
                        }
                        choice.chosen = chosen;
                        choice.confidence = Probability::new(1.0)?;
                    }
                }
                DecisionAnswer::Noul(noul) => {
                    noul.probability_true = Probability::new(
                        if work || id == &QuestionId::global(GlobalQuestion::EvidenceSufficient) {
                            1.0
                        } else {
                            0.0
                        },
                    )?;
                }
                DecisionAnswer::Score(score) => {
                    let gain = id.as_str().ends_with("gain") && (work || !id.as_str().starts_with("candidate."));
                    let chosen = if gain {
                        score.distribution.len().saturating_sub(1)
                    } else {
                        0
                    };
                    for (index, probability) in score.distribution.iter_mut().enumerate() {
                        *probability = Probability::new(if index == chosen { 1.0 } else { 0.0 })?;
                    }
                    score.expected = f64::from(u32::try_from(chosen).expect("bounded score"));
                    score.confidence = Probability::new(1.0)?;
                }
            }
        }
        Ok(response)
    }
}

#[tokio::test]
async fn actual_failed_ledger_terminal_replay_preserves_facts_into_next_segment() -> anyhow::Result<()> {
    child_source_gate_and_ledger_case().await
}

#[tokio::test]
async fn non_agentic_delegate_child_does_not_publish_or_bind_an_always_include_tool() -> anyhow::Result<()> {
    child_source_gate_and_ledger_case().await
}

async fn child_source_gate_and_ledger_case() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let owned_runs = Arc::new(tokio::sync::RwLock::new(Vec::<SubAgentRun>::new()));
    let workspace = scratch.path().join("logical");
    let config_dir = scratch.path().join("config");
    let private_home = scratch.path().join("worker-home");
    let workers = scratch.path().join("workers");
    for path in [&workspace, &config_dir, &private_home, &workers] {
        std::fs::create_dir_all(path)?;
    }
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.clone();
    config.config_path = config_dir.join("config.toml");
    config.default_provider = Some("ollama".into());
    config.default_model = Some("prx-local-model".into());
    config.memory.backend = "sqlite".into();
    config.causal_tree.enabled = true;
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.sessions_spawn.default_mode = "process".into();
    config.sessions_spawn.process_memory_strategy = "isolated_private".into();
    config.sessions_spawn.worker_workspace_root = Some(workers.to_string_lossy().into_owned());
    config.sessions_spawn.cleanup_on_complete = false;
    config.tool_tiering.always_include.push("file_read".to_owned());
    config.agents.insert(
        "prx_reader".to_owned(),
        crate::config::DelegateAgentConfig {
            provider: "ollama".to_owned(),
            model: "prx-local-model".to_owned(),
            system_prompt: None,
            api_key: None,
            credential_ref: None,
            temperature: None,
            max_depth: 1,
            agentic: false,
            allowed_tools: Vec::new(),
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: None,
        },
    );
    config.providers.models.push(toml::from_str(
        "provider='ollama'\nmodel='prx-local-model'\noperations=['chat']\nnative_tools=true\n",
    )?);
    let resolved = crate::providers::selection::check(
        &config,
        "ollama",
        "prx-local-model",
        &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    assert_eq!(
        resolved.verification(),
        &crate::providers::catalog::Verification::Verified
    );
    std::fs::write(&config.config_path, toml::to_string(&config)?)?;
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &workspace));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&workspace)?);
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let spawn = SessionsSpawnTool::new_with_registry(
        Arc::new(NoAnnouncements),
        Arc::new(NoTransport),
        "ollama",
        "prx-local-model",
        0.0,
        Arc::clone(&security),
        workspace.clone(),
        config.multimodal.clone(),
        config.agent.compaction.clone(),
        Default::default(),
        None,
        crate::providers::ProviderRuntimeOptions::default(),
        config.sessions_spawn.clone(),
        Arc::clone(&owned_runs),
    )
    .with_hint_config(&config)
    .with_model_catalog(crate::providers::selection::catalog_for(&config))
    .with_shared_memory(Arc::clone(&memory))
    .with_source_fabric(fabric)
    .with_event_recording(config.memory.event_recording_config());
    let tools_handle = spawn.tools_handle();
    let physical_calls = Arc::new(AtomicUsize::new(0));
    let hidden_calls = Arc::new(AtomicUsize::new(0));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(spawn),
        Box::new(crate::chat::scheduled_input::ScheduledInputTool::new(
            crate::chat::scheduled_input::ScheduledInputHandle::default(),
        )),
        Box::new(CountedRead {
            inner: crate::tools::file_read::FileReadTool::new(Arc::clone(&security), false),
            calls: Arc::clone(&physical_calls),
        }),
        Box::new(HiddenChildCapability {
            calls: Arc::clone(&hidden_calls),
        }),
    ]);
    assert!(tools_handle.set(&registry));
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::from([
                    "sessions_spawn".into(),
                    "chat_schedule".into(),
                    "file_read".into(),
                    HIDDEN_CHILD_CAPABILITY.into(),
                ]),
                ..SurfacePolicy::default()
            },
            provider_catalog: &catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )?;
    let owning = Arc::get_mut(&mut service).expect("unshared parent service");
    owning.factory = DecisionBackendFactory::from_slots(
        vec![Box::new(ParentDelegateBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-local-model"),
            },
        })],
        Arc::clone(&owning.state_key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        "sessions_spawn: Read file_read project metadata, then use chat_schedule.",
        "prx_parent_process_component",
    )
    .await;
    let cancellation = CancellationToken::new();
    let context = service
        .decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation)
        .await?;

    assert!(matches!(context.directive(), TurnExecutionDirective::Delegate { .. }));
    let grant = service.issue_child_grant(&proof, &context)?;
    // A child can only use work named in the actual admitted parent request.
    // Registering another capability on both executors never grants it.
    let (issued, _) = grant.scope().gate_inputs();
    let read = grant
        .catalog()
        .entry_for_public_name("file_read")
        .expect("registered read");
    let schedule = grant
        .catalog()
        .entry_for_public_name("chat_schedule")
        .expect("registered interactive-only scheduler");
    let hidden = grant
        .catalog()
        .entry_for_public_name(HIDDEN_CHILD_CAPABILITY)
        .expect("registered hidden child capability");
    assert_eq!(issued.allowed.len(), 2, "grant narrows to the two requested tools");
    assert!(issued.allowed.iter().any(|reference| {
        reference.id == read.id && reference.fingerprint == read.fingerprint && reference.generation == read.generation
    }));
    assert!(
        issued.allowed.iter().all(|reference| reference.id != hidden.id),
        "the sealed manifest must not inherit an unrequested registered capability"
    );
    assert!(issued.allowed.iter().any(|reference| reference.id == schedule.id));
    let grant = Arc::new(grant.for_process_worker()?);
    let (worker_issued, _) = grant.scope().gate_inputs();
    assert_eq!(worker_issued.allowed.len(), 1);
    assert!(worker_issued.allowed.iter().any(|reference| reference.id == read.id));
    assert!(
        worker_issued
            .allowed
            .iter()
            .all(|reference| reference.id != schedule.id)
    );
    let binding = ChildProviderBinding::build(
        Arc::clone(&grant),
        "ollama",
        "prx-local-model",
        service.credential_for_proof(&proof),
    )?;
    let child_provider = binding.provider();
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let mut task_scope =
        crate::memory::MessageEventScope::new("sessions_spawn", crate::memory::MemoryVisibility::Workspace)
            .with_session_key("prx_g8_child_session")
            .with_run_id("prx_g8_task");
    grant.bind_event_scope(&mut task_scope)?;
    let task_input = crate::runtime::child_source::record_child_input(
        &fabric,
        task_scope,
        "Read file_read project metadata",
        Some("prx_g8_task:input".into()),
        None,
        Some(&grant),
    )
    .await?
    .expect("real task input receipt");
    let mut sequence = ChildSegmentSequence::new(Arc::clone(&grant), task_input)?;
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(CountedRead {
            inner: crate::tools::file_read::FileReadTool::new(Arc::clone(grant.security()), false),
            calls: Arc::clone(&physical_calls),
        }),
        Box::new(HiddenChildCapability {
            calls: Arc::clone(&hidden_calls),
        }),
    ]);
    let execution = Arc::new(
        ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(grant.security()))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        )
        .with_idempotency_memory(Arc::clone(&memory)),
    );
    let mut child_service = build_turn_decision_service(
        TurnSurfaceKind::Worker,
        TurnServiceBuild {
            parent_grant: Some(Arc::clone(&grant)),
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::from(["file_read".into(), HIDDEN_CHILD_CAPABILITY.into()]),
                ..SurfacePolicy::default()
            },
            provider_catalog: &catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security: Arc::clone(grant.security()),
        },
    )?;
    let owning = Arc::get_mut(&mut child_service).expect("private child before publication");
    owning.factory = DecisionBackendFactory::from_slots(
        vec![Box::new(ReadBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-local-model"),
            },
        })],
        Arc::clone(&owning.state_key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let mut restricted_scope =
        crate::memory::MessageEventScope::new("delegate", crate::memory::MemoryVisibility::Workspace)
            .with_session_key("prx_non_agentic_session")
            .with_run_id("prx_non_agentic_run")
            .with_agent_id("prx_reader");
    grant.bind_event_scope(&mut restricted_scope)?;
    let restricted_input = crate::runtime::child_source::record_child_input(
        &fabric,
        restricted_scope,
        "Hello",
        Some("prx_non_agentic_run:input".into()),
        None,
        Some(&grant),
    )
    .await?
    .expect("real non-agentic delegate input receipt");
    let restricted_proof = binding.source_proof(&grant, &restricted_input, &child_provider)?;
    assert!(
        !crate::runtime::turn_source::TurnSourceProof::tool_calls_allowed(&restricted_proof),
        "the configured non-agentic Delegate source must disable tool calls"
    );
    let restricted_context = child_service
        .decide_agent_initial(&restricted_proof, AgentTurnKind::Repl, &cancellation)
        .await?;
    assert!(
        matches!(restricted_context.directive(), TurnExecutionDirective::AnswerDirectly),
        "non-agentic plain turn did not answer: directive={:?}; selection={:?}; fallback={:?}; candidates={:?}",
        restricted_context.directive(),
        restricted_context.engine.selection,
        restricted_context.engine.fallback_cause,
        restricted_context.engine.candidates,
    );
    assert!(
        restricted_context.execution_refs().is_empty(),
        "a source without tool-call authority must publish no direct references"
    );
    let restricted_specs = child_service.agent_execution_specs(&restricted_proof, &restricted_context)?;
    assert!(restricted_specs.specs.is_empty());
    let restricted_execution = restricted_input.execution_context(&restricted_proof)?;
    let forged_spec = ToolSpec {
        name: "file_read".to_owned(),
        description: "must be rejected before schema binding".to_owned(),
        parameters: serde_json::json!({"type":"object"}),
    };
    let bind_error = match child_service.bind_agent_execution(
        &restricted_proof,
        &restricted_context,
        &restricted_execution,
        &[forged_spec],
    ) {
        Ok(_) => anyhow::bail!("a caller re-added a tool excluded by source authority"),
        Err(error) => error,
    };
    assert_eq!(bind_error.to_string(), "source configuration forbids tool calls");
    restricted_context.terminal_execution_trace();
    let owner = Arc::new(CleanupTracker::default());
    // Catch only the fixture work. Its future drops terminal guards before
    // cleanup starts; errors and panics cannot delete SQLite under owned writes.
    let outcome = std::panic::AssertUnwindSafe(async {
        let collector = Arc::new(TerminalUsageCollector::new(
            workspace.to_string_lossy().into_owned(),
            proof.owner_id().into(),
            "prx_g8_task".into(),
        ));
        let receipt = sequence.record(&fabric, "Read file_read project metadata").await?;
        let child_proof = Arc::new(binding.source_proof(&grant, &receipt, &child_provider)?);
        let execution_context = receipt.execution_context(&child_proof)?;
        let first = ChildSegmentTurn::admit(
            Arc::clone(&child_service),
            (Arc::clone(&child_proof), execution_context.clone()),
            fabric.clone(),
            Arc::new(config.clone()),
            &owner,
            &cancellation,
            Some(Arc::clone(&collector)),
        )
        .await?;
        let routing = first.routing();
        assert!(
            matches!(routing.context.directive(), TurnExecutionDirective::Execute { .. }),
            "child Execute refused: selection={:?}; fallback={:?}; candidates={:?}; scores={:?}; post={:?}; final={:?}",
            routing.context.engine.selection,
            routing.context.engine.fallback_cause,
            routing.context.engine.candidates,
            routing.context.engine.scores,
            routing.context.engine.post_rehearsal,
            routing.context.engine.pre_commit,
        );
        let original = [
            routing
                .context
                .engine
                .state
                .material_fingerprint(&child_service.state_key),
            routing
                .context
                .engine
                .state
                .record_fingerprint(&child_service.state_key),
        ];
        assert!(
            routing
                .context
                .engine
                .state
                .observed
                .exact_call_observations()
                .is_none()
        );
        let mut no_facts = routing.context.engine.state.clone();
        no_facts.observed = no_facts.observed.clone().with_exact_call_observations(None);
        assert_eq!(
            original,
            [
                no_facts.material_fingerprint(&child_service.state_key),
                no_facts.record_fingerprint(&child_service.state_key)
            ]
        );
        let legacy =
            serde_json::to_string(&crate::tools::capability::execution_trace::ExecutionTraceSnapshot::default())?;
        assert!(!legacy.contains("exact_call_observations"));
        let roundtrip: crate::tools::capability::execution_trace::ExecutionTraceSnapshot =
            serde_json::from_str(&legacy)?;
        assert_eq!(legacy, serde_json::to_string(&roundtrip)?);
        let specs = child_service.agent_execution_specs(child_proof.as_ref(), routing.context)?;
        assert_eq!(
            specs.specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["file_read"]
        );
        assert!(
            routing
                .context
                .execution_refs()
                .iter()
                .all(|reference| reference.id != hidden.id),
            "the child engine must not publish the registered capability omitted from its sealed manifest"
        );
        let hidden_descriptor = child_service
            .execution
            .current_descriptor(HIDDEN_CHILD_CAPABILITY)
            .expect("the hidden backend remains registered for the negative control");
        let hidden_spec = ToolSpec {
            name: hidden_descriptor.public_name,
            description: hidden_descriptor.description,
            parameters: hidden_descriptor.parameters,
        };
        let bind_error = match child_service.bind_agent_execution(
            child_proof.as_ref(),
            routing.context,
            &execution_context,
            &[hidden_spec],
        ) {
            Ok(_) => anyhow::bail!("the child rebound a capability outside its sealed manifest"),
            Err(error) => error,
        };
        assert_eq!(bind_error.to_string(), "unknown runtime dependency");
        let (executor, bound) = child_service.bind_agent_execution(
            child_proof.as_ref(),
            routing.context,
            &execution_context,
            &specs.specs,
        )?;
        let command = || {
            ToolExecutionCommand::new("file_read", serde_json::json!({"path":"prx-missing-input.txt"}))
                .with_idempotency_key("prx_exact_read")
        };
        let physical_command = command();
        let cached_command = command();
        assert_ne!(physical_command.operation_id, cached_command.operation_id);
        assert_eq!(physical_command.idempotency_key, cached_command.idempotency_key);
        assert_eq!(physical_command.arguments, cached_command.arguments);
        let expected_fingerprint = |command: &ToolExecutionCommand| {
            use sha2::Digest;
            let input_sha256 = format!("{:x}", sha2::Sha256::digest(command.arguments.to_string().as_bytes()));
            let envelope = &bound.envelope;
            child_service.state_key.hmac(
                crate::decision::KeyDomain::StateFingerprint,
                &[
                    b"exact-tool-ledger-observation.v1",
                    envelope.workspace_id.as_bytes(),
                    &[u8::from(envelope.owner_id.is_some())],
                    envelope.owner_id.as_deref().unwrap_or_default().as_bytes(),
                    envelope.session_key.as_bytes(),
                    bound.semantic_turn_id.as_bytes(),
                    command
                        .idempotency_key
                        .as_deref()
                        .expect("actual shared ledger key")
                        .as_bytes(),
                    command.operation_id.as_bytes(),
                    command.name.as_bytes(),
                    input_sha256.as_bytes(),
                    &[u8::from(envelope.config_generation_id.is_some())],
                    &envelope.config_generation_id.unwrap_or_default().to_le_bytes(),
                    envelope
                        .config_source_revision
                        .as_deref()
                        .unwrap_or_default()
                        .as_bytes(),
                ],
            )
        };
        let physical_fingerprint = expected_fingerprint(&physical_command);
        let cached_fingerprint = expected_fingerprint(&cached_command);
        let failed = executor
            .execute(physical_command, bound.clone(), Some(cancellation.clone()))
            .await;
        assert_eq!(failed.status, ToolExecutionStatus::Failed);
        assert!(!failed.replayed);
        let cached = executor
            .execute(cached_command, bound, Some(cancellation.clone()))
            .await;
        assert_eq!(cached.status, ToolExecutionStatus::Failed);
        assert!(cached.replayed);
        assert_eq!(physical_calls.load(Ordering::SeqCst), 1);
        first.fail(&anyhow::anyhow!("actual file read failed"), false).await?;
        let terminal = memory
            .find_message_event_by_idempotency_key(
                &workspace.to_string_lossy(),
                &format!("turn:{}:final", child_proof.run_id()),
            )
            .await?
            .expect("durable failed terminal");
        let payload: serde_json::Value =
            serde_json::from_str(terminal.raw_payload_json.as_deref().expect("terminal payload"))?;
        let trace = serde_json::from_value::<crate::tools::capability::execution_trace::ExecutionTraceSnapshot>(
            payload.get("execution_trace").expect("real execution trace").clone(),
        )?;
        let facts = trace
            .exact_call_observations
            .as_ref()
            .expect("real exact-call ledger facts");
        assert_eq!(facts.physical_failure_count(), 1);
        assert_eq!(facts.cached_return_count(), 1);
        assert_eq!(facts.calls.len(), 2);
        assert_eq!(
            facts.calls.first().expect("physical call").fingerprint,
            physical_fingerprint
        );
        assert_eq!(facts.calls.last().expect("cached call").fingerprint, cached_fingerprint);
        assert_ne!(physical_fingerprint, cached_fingerprint);
        let store_workspace = child_service.store.workspace_id().expect("durable decision store");
        let reader = child_service.store.reader();
        crate::agent::terminal::verify_terminal_replay(
            memory.as_ref(),
            &reader,
            &child_service.state_key,
            &store_workspace,
            &workspace.to_string_lossy(),
            proof.owner_id(),
            child_proof.run_id(),
            &terminal,
        )
        .await?;
        let mut tampered = terminal.clone();
        let mut corrupt = payload.clone();
        let fingerprint = corrupt
            .get_mut("execution_trace")
            .and_then(|v| v.get_mut("exact_call_observations"))
            .and_then(|v| v.get_mut("calls"))
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|v| v.first_mut())
            .and_then(|v| v.get_mut("fingerprint"))
            .expect("actual issued fingerprint");
        *fingerprint = serde_json::to_value([0_u8; 32])?;
        tampered.raw_payload_json = Some(serde_json::to_string(&corrupt)?);
        assert!(
            crate::agent::terminal::verify_terminal_replay(
                memory.as_ref(),
                &reader,
                &child_service.state_key,
                &store_workspace,
                &workspace.to_string_lossy(),
                proof.owner_id(),
                child_proof.run_id(),
                &tampered
            )
            .await
            .is_err()
        );
        let next_receipt = sequence.record(&fabric, "Read file_read next project metadata").await?;
        let next_proof = Arc::new(binding.source_proof(&grant, &next_receipt, &child_provider)?);
        let next_execution = next_receipt.execution_context(&next_proof)?;
        let next = ChildSegmentTurn::admit(
            Arc::clone(&child_service),
            (next_proof, next_execution),
            fabric,
            Arc::new(config),
            &owner,
            &cancellation,
            Some(collector),
        )
        .await?;
        let next_routing = next.routing();
        let inherited = next_routing
            .context
            .engine
            .state
            .observed
            .exact_call_observations()
            .expect("confirmed predecessor facts");
        assert_eq!(inherited.as_ref(), facts.as_ref());
        assert_eq!(inherited.physical_failure_count(), 1);
        assert_eq!(inherited.cached_return_count(), 1);
        assert_eq!(
            physical_calls.load(Ordering::SeqCst),
            1,
            "inheritance never executes a tool"
        );
        assert_eq!(
            hidden_calls.load(Ordering::SeqCst),
            0,
            "the registered capability outside the sealed child manifest never executes"
        );
        next.fail(&anyhow::anyhow!("fixture closes without another tool dispatch"), false)
            .await?;
        assert!(owned_runs.read().await.is_empty(), "no worker process is started");
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    let owner_result = owner.drain(tokio::time::Instant::from_std(cleanup_deadline)).await;
    // Both queues receive the same absolute cleanup deadline even if one fails.
    let child_drained = child_service.drain_writes(cleanup_deadline).await;
    let parent_drained = service.drain_writes(cleanup_deadline).await;
    if owner_result.is_err() || !child_drained || !parent_drained {
        let preserved = scratch.keep();
        tracing::error!(path = %preserved.display(), "G8 fixture cleanup unconfirmed; scratch preserved");
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
        if let Ok(Err(error)) = outcome {
            return Err(error.context("G8 fixture cleanup also unconfirmed; scratch preserved"));
        }
        anyhow::bail!("G8 fixture cleanup unconfirmed; scratch preserved");
    }
    match outcome {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
