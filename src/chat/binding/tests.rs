#![cfg(test)]
mod operator_accounting;
#[cfg(unix)]
mod shell_parity_working_root;
#[cfg(unix)]
mod worker_stdin_protocol;
use super::*;
use crate::agent::terminal::TurnTerminalStatus;
use crate::memory::{MemoryFabric, SqliteMemory};

#[test]
fn failed_inbound_persistence_names_the_cause_and_hides_private_text() {
    let error =
        anyhow::anyhow!("invalid ingress capsule authentication").context("owner=/Users/hidden/prx_owner token=abcd");
    let surfaced = chat_input_persistence_failure(&error);
    let text = surfaced.to_string();
    assert!(text.contains("invalid ingress capsule authentication"), "{text}");
    assert!(
        !text.contains("enabled chat decision source was not recorded"),
        "{text}"
    );
    assert!(!text.contains("hidden"), "{text}");
    assert!(!text.contains("abcd"), "{text}");
    assert!(!text.contains("token"), "{text}");
}

/// Admit only an actual local `/bg` fixture through Chat's private source path.
pub(crate) async fn recorded_local_operator_spawn(
    publication: crate::runtime::turn_publication::TurnPublication,
    provider: Arc<dyn crate::providers::Provider>,
    task: &str,
) -> anyhow::Result<(serde_json::Value, crate::runtime::operator_spawn::OperatorSpawnSource)> {
    let generation = Arc::clone(&publication.generation);
    let memory = Arc::clone(&publication.memory);
    let security = Arc::clone(&publication.security);
    let config = &generation.effective;
    let provider_name = config
        .default_provider
        .as_deref()
        .context("operator fixture provider missing")?;
    let model_name = config
        .default_model
        .as_deref()
        .context("operator fixture model missing")?;
    let binding = ChatProviderBinding::capture(&generation, &provider, provider_name, model_name)?
        .context("operator fixture provider binding missing")?;
    let service = publication
        .publish(crate::tools::capability::turn_state::TurnSurfaceKind::Interactive)
        .await?;
    crate::runtime::turn_cleanup::current_tracker()
        .context("operator fixture cleanup owner missing")?
        .retain_service(Arc::clone(&service));
    let message = crate::channels::traits::ChannelMessage {
        id: "prx-operator-command".to_owned(),
        sender: "user".to_owned(),
        channel: "terminal".to_owned(),
        content: format!("/bg {task}"),
        ..Default::default()
    };
    ChatOperatorSpawn::capture(
        &message,
        task,
        &binding,
        &generation,
        &provider,
        provider_name,
        model_name,
    )?
    .admit(memory, "prx_operator_session", security, service)
    .await
}

/// Record and admit a model-origin turn before dispatching its spawn call.
/// This preserves the configured decision policy, including an abstention.
pub(crate) async fn dispatch_recorded_model_spawn(
    publication: crate::runtime::turn_publication::TurnPublication,
    provider: Arc<dyn crate::providers::Provider>,
    arguments: serde_json::Value,
    serving_phase: Arc<std::sync::atomic::AtomicU8>,
) -> anyhow::Result<crate::tools::ToolResult> {
    use crate::runtime::turn_source::TurnSourceProof as _;
    let generation = Arc::clone(&publication.generation);
    let memory = Arc::clone(&publication.memory);
    let config = &generation.effective;
    let provider_name = config
        .default_provider
        .as_deref()
        .context("model fixture provider missing")?;
    let model_name = config.default_model.as_deref().context("model fixture model missing")?;
    let selected = ChatProviderBinding::capture(&generation, &provider, provider_name, model_name)?
        .context("model fixture provider binding missing")?;
    let service = publication
        .publish(crate::tools::capability::turn_state::TurnSurfaceKind::Interactive)
        .await?;
    crate::runtime::turn_cleanup::current_tracker()
        .context("model fixture cleanup owner missing")?
        .retain_service(Arc::clone(&service));
    let input = "spawn model child";
    let session = "chat:session-1";
    let run = "turn-run-xyz";
    let envelope = super::super::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), session)
        .with_config_generation(&generation)
        .with_run_id(run.to_owned());
    let fabric = MemoryFabric::new(Arc::clone(&memory), config.workspace_dir.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let event = fabric
        .record_inbound_user_message(envelope.message_scope(), input, None, None)
        .await?;
    let receipt = ChatSourceReceipt::issue(memory.as_ref(), &generation, session, run, input, &event).await?;
    let binding = receipt.bind_provider(
        &selected,
        ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
    )?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let context = binding.issue_decision(&service, &cancellation).await?;
    use crate::tools::Tool as _;
    let mut specs = service.agent_execution_specs(binding.as_ref(), context)?;
    if !specs.specs.iter().any(|spec| spec.name == "sessions_spawn") {
        let search = service.capability_search_port(binding.as_ref(), context)?;
        let found = crate::tools::capability::search_tool::with_search_authority(
            Some(search),
            crate::tools::capability::search_tool::CapabilitySearchTool
                .execute(serde_json::json!({"query":"sessions_spawn","limit":1})),
        )
        .await?;
        anyhow::ensure!(found.success, "source-owned spawn search failed");
        service.admit_offered_discoveries(binding.as_ref(), context, |_, _| false);
        specs
            .specs
            .extend(service.agent_appended_execution_specs(binding.as_ref(), context)?.specs);
    }
    anyhow::ensure!(
        specs.specs.iter().any(|spec| spec.name == "sessions_spawn"),
        "genuine model admission did not select sessions_spawn"
    );
    let execution_context =
        crate::tools::ToolExecutionContext::new(binding.envelope().clone(), binding.policy_chat_type());
    let (execution, execution_context) =
        service.bind_agent_execution(binding.as_ref(), context, &execution_context, &specs.specs)?;
    let messages = [crate::providers::ChatMessage::user(input)];
    let response = provider
        .chat(
            crate::providers::ChatRequest {
                messages: &messages,
                tools: Some(&specs.specs),
            },
            model_name,
            0.7,
        )
        .await?;
    anyhow::ensure!(response.tool_calls.len() == 1, "one genuine parent tool call required");
    let call = response.tool_calls.into_iter().next().context("parent call missing")?;
    anyhow::ensure!(call.name == "sessions_spawn", "unexpected parent tool name");
    let mut returned_arguments: serde_json::Value = serde_json::from_str(&call.arguments)?;
    anyhow::ensure!(
        returned_arguments == serde_json::json!({"task":"model child"}),
        "parent arguments changed"
    );
    let grant_key = crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG;
    let grant = arguments.get(grant_key).context("host resource grant missing")?.clone();
    returned_arguments
        .as_object_mut()
        .context("parent object required")?
        .insert(grant_key.to_owned(), grant);
    serving_phase
        .compare_exchange(
            1,
            2,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .map_err(|_| anyhow::anyhow!("parent response phase was not observed"))?;
    let outcome = execution
        .execute(
            crate::tools::ToolExecutionCommand::new(call.name, returned_arguments),
            execution_context,
            Some(cancellation),
        )
        .await;
    match (outcome.result, outcome.error) {
        (Some(result), _) => Ok(result),
        (None, Some(error)) => Err(anyhow::anyhow!(error)),
        (None, None) => Err(anyhow::anyhow!(
            "genuine model dispatch ended with status {:?} and no tool result",
            outcome.status
        )),
    }
}

/// Record and admit a model-origin turn before dispatching its spawn call.
/// This preserves the configured decision policy, including an abstention.
pub(crate) async fn dispatch_unknown_agent_fixture(
    publication: crate::runtime::turn_publication::TurnPublication,
    provider: Arc<dyn crate::providers::Provider>,
    arguments: serde_json::Value,
) -> anyhow::Result<crate::tools::ToolResult> {
    use crate::runtime::turn_source::TurnSourceProof as _;
    let generation = Arc::clone(&publication.generation);
    let memory = Arc::clone(&publication.memory);
    let config = &generation.effective;
    let provider_name = config
        .default_provider
        .as_deref()
        .context("model fixture provider missing")?;
    let model_name = config.default_model.as_deref().context("model fixture model missing")?;
    let selected = ChatProviderBinding::capture(&generation, &provider, provider_name, model_name)?
        .context("model fixture provider binding missing")?;
    let service = publication
        .publish(crate::tools::capability::turn_state::TurnSurfaceKind::Interactive)
        .await?;
    crate::runtime::turn_cleanup::current_tracker()
        .context("model fixture cleanup owner missing")?
        .retain_service(Arc::clone(&service));
    let input = "sessions_spawn hello";
    let session = "chat:unknown-agent";
    let run = "prx_unknown_agent_parent";
    let envelope = super::super::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), session)
        .with_config_generation(&generation)
        .with_run_id(run.to_owned());
    let fabric = MemoryFabric::new(Arc::clone(&memory), config.workspace_dir.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let event = fabric
        .record_inbound_user_message(envelope.message_scope(), input, None, None)
        .await?;
    let receipt = ChatSourceReceipt::issue(memory.as_ref(), &generation, session, run, input, &event).await?;
    let binding = receipt.bind_provider(
        &selected,
        ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
    )?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let context = binding.issue_decision(&service, &cancellation).await?;
    use crate::tools::Tool as _;
    let mut specs = service.agent_execution_specs(binding.as_ref(), context)?;
    if !specs.specs.iter().any(|spec| spec.name == "sessions_spawn") {
        let search = service.capability_search_port(binding.as_ref(), context)?;
        let found = crate::tools::capability::search_tool::with_search_authority(
            Some(search),
            crate::tools::capability::search_tool::CapabilitySearchTool
                .execute(serde_json::json!({"query":"sessions_spawn","limit":1})),
        )
        .await?;
        anyhow::ensure!(found.success, "source-owned spawn search failed");
        service.admit_offered_discoveries(binding.as_ref(), context, |_, _| false);
        specs
            .specs
            .extend(service.agent_appended_execution_specs(binding.as_ref(), context)?.specs);
    }
    anyhow::ensure!(
        specs.specs.iter().any(|spec| spec.name == "sessions_spawn"),
        "genuine model admission did not select sessions_spawn"
    );
    let execution_context =
        crate::tools::ToolExecutionContext::new(binding.envelope().clone(), binding.policy_chat_type());
    let (execution, execution_context) =
        service.bind_agent_execution(binding.as_ref(), context, &execution_context, &specs.specs)?;
    let outcome = execution
        .execute(
            crate::tools::ToolExecutionCommand::new("sessions_spawn", arguments),
            execution_context,
            Some(cancellation),
        )
        .await;
    match (outcome.result, outcome.error) {
        (Some(result), _) => Ok(result),
        (None, Some(error)) => Err(anyhow::anyhow!(error)),
        (None, None) => Err(anyhow::anyhow!(
            "genuine model dispatch ended with status {:?} and no tool result",
            outcome.status
        )),
    }
}

#[tokio::test]
async fn chat_production_save_excludes_live_input_without_useful_feedback() {
    let (_dir, fabric, binding) = fixture(true).await;
    let key = crate::agent::context_recall::current_input_memory_key(binding.envelope().run_id.as_deref().unwrap());
    crate::agent::entry::save_current_input(
        &fabric,
        binding.envelope().run_id.as_deref().unwrap(),
        &binding.envelope().session_key,
        "prx input",
        binding.envelope().source_message_event_id.as_deref(),
    )
    .await
    .unwrap();
    let memory = fabric.memory();
    let before = memory.get(&key).await.unwrap().unwrap();
    assert_eq!(
        before.session_id.as_deref(),
        Some(binding.envelope().session_key.as_str())
    );
    let recalled = binding.recalled_context(memory.as_ref()).await;
    assert!(!recalled.preamble.contains("[Memory context]"));
    assert!(!recalled.ids.contains(&before.id));
    crate::agent::context_recall::increment_recalled_useful_counts(memory.as_ref(), &recalled.ids).await;
    assert_eq!(
        memory.get(&key).await.unwrap().unwrap().useful_count,
        before.useful_count
    );
}

async fn fixture(record_input: bool) -> (tempfile::TempDir, MemoryFabric, Arc<ChatTurnBinding>) {
    fixture_with_advisory(record_input, Default::default()).await
}

pub(in crate::chat) async fn fixture_with_advisory(
    record_input: bool,
    advisory: crate::config::schema::decision_engine::DecisionEngineConfig,
) -> (tempfile::TempDir, MemoryFabric, Arc<ChatTurnBinding>) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.decision_engine = advisory;
    config.workspace_dir = dir.path().to_path_buf();
    config.memory.events.record_user_messages = record_input;
    let generation = crate::config::new_shared(config).pin();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
    let fabric = MemoryFabric::new(memory.clone(), dir.path().to_string_lossy())
        .with_event_recording(generation.effective.memory.event_recording_config());
    let session = super::super::session::ChatSession::new("ollama", "prx-local-model");
    let event = super::super::record_chat_user_message_event_in_generation(
        &fabric,
        &session,
        "prx_session",
        "prx_run",
        "ollama",
        "prx-local-model",
        1,
        "prx input",
        Some(&generation),
    )
    .await
    .unwrap();
    let source = ChatSourceReceipt::issue_for_turn(
        memory.as_ref(),
        &generation,
        "prx_session",
        "prx_run",
        "prx input",
        Some(&event),
    )
    .await
    .unwrap();
    let provider: Arc<dyn crate::providers::Provider> =
        Arc::new(crate::providers::ollama::OllamaProvider::new(None, None));
    let provider_binding = ChatProviderBinding::capture(&generation, &provider, "ollama", "prx-local-model")
        .unwrap()
        .unwrap();
    let binding = source
        .bind_provider(
            &provider_binding,
            ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
        )
        .unwrap();
    (dir, fabric, binding)
}

pub(in crate::chat) async fn decided_binding(
    fabric: &MemoryFabric,
    session_key: &str,
    run_id: &str,
) -> (
    Arc<ChatTurnBinding>,
    Arc<crate::runtime::turn_publication::TurnRuntimeService>,
) {
    let (binding, service, _) = decided_binding_for_input(fabric, session_key, run_id, "prx fixture input").await;
    (binding, service)
}

pub(in crate::chat) async fn decided_binding_for_input(
    fabric: &MemoryFabric,
    session_key: &str,
    run_id: &str,
    input: &str,
) -> (
    Arc<ChatTurnBinding>,
    Arc<crate::runtime::turn_publication::TurnRuntimeService>,
    MessageEvent,
) {
    decided_binding_for_provider(fabric, session_key, run_id, input, "ollama", "prx-local-model").await
}

pub(in crate::chat) async fn decided_binding_for_provider(
    fabric: &MemoryFabric,
    session_key: &str,
    run_id: &str,
    input: &str,
    provider_name: &str,
    model_name: &str,
) -> (
    Arc<ChatTurnBinding>,
    Arc<crate::runtime::turn_publication::TurnRuntimeService>,
    MessageEvent,
) {
    decided_binding_for_provider_retained(fabric, session_key, run_id, input, provider_name, model_name, None).await
}

pub(in crate::chat) struct FixtureServiceDrain {
    service: Option<Arc<crate::runtime::turn_publication::TurnRuntimeService>>,
    ticket: Option<crate::runtime::turn_cleanup::CleanupTicket>,
}

impl Drop for FixtureServiceDrain {
    fn drop(&mut self) {
        let Some(service) = self.service.take() else {
            return;
        };
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let drained = service
                    .drain_writes(std::time::Instant::now() + std::time::Duration::from_secs(2))
                    .await;
                ticket.finish(drained);
            });
        }
        // Without a live runtime, dropping the unfinished ticket fails cleanup.
    }
}

pub(in crate::chat) async fn decided_binding_for_input_retained(
    fabric: &MemoryFabric,
    session_key: &str,
    run_id: &str,
    input: &str,
    retained: &mut Option<FixtureServiceDrain>,
) -> (
    Arc<ChatTurnBinding>,
    Arc<crate::runtime::turn_publication::TurnRuntimeService>,
    MessageEvent,
) {
    decided_binding_for_provider_retained(
        fabric,
        session_key,
        run_id,
        input,
        "ollama",
        "prx-local-model",
        Some(retained),
    )
    .await
}

async fn decided_binding_for_provider_retained(
    fabric: &MemoryFabric,
    session_key: &str,
    run_id: &str,
    input: &str,
    provider_name: &str,
    model_name: &str,
    retained: Option<&mut Option<FixtureServiceDrain>>,
) -> (
    Arc<ChatTurnBinding>,
    Arc<crate::runtime::turn_publication::TurnRuntimeService>,
    MessageEvent,
) {
    let mut config = crate::config::Config::default();
    config.workspace_dir = std::path::PathBuf::from(fabric.workspace_id());
    config.config_path = config.workspace_dir.join("config.toml");
    config.default_provider = Some(provider_name.to_owned());
    config.default_model = Some(model_name.to_owned());
    if provider_name == "anthropic" {
        config.api_key = Some("prx-test-only-credential".to_owned());
    }
    let generation = crate::config::new_shared(config).pin();
    let session = super::super::session::ChatSession::new(provider_name, model_name);
    let event = super::super::record_chat_user_message_event_in_generation(
        fabric,
        &session,
        session_key,
        run_id,
        provider_name,
        model_name,
        1,
        input,
        Some(&generation),
    )
    .await
    .unwrap();
    let source = ChatSourceReceipt::issue(
        fabric.memory().as_ref(),
        &generation,
        session_key,
        run_id,
        input,
        &event,
    )
    .await
    .unwrap();
    let provider: Arc<dyn crate::providers::Provider> = match provider_name {
        "ollama" => Arc::new(crate::providers::ollama::OllamaProvider::new(None, None)),
        "anthropic" => Arc::new(crate::providers::anthropic::AnthropicProvider::new(
            generation.effective.api_key.as_deref(),
        )),
        _ => panic!("unsupported Chat fixture provider"),
    };
    let provider_binding = ChatProviderBinding::capture(&generation, &provider, provider_name, model_name)
        .unwrap()
        .unwrap();
    let binding = source
        .bind_provider(
            &provider_binding,
            ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
        )
        .unwrap();
    let service = service(fabric, &binding).await;
    if let Some(retained) = retained {
        let tracker = crate::runtime::turn_cleanup::current_tracker().expect("owned fixture cleanup tracker");
        let ticket = tracker
            .register(tokio_util::sync::CancellationToken::new())
            .expect("owned fixture service ticket");
        *retained = Some(FixtureServiceDrain {
            service: Some(Arc::clone(&service)),
            ticket: Some(ticket),
        });
    }
    binding
        .issue_decision(&service, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    (binding, service, event)
}

pub(in crate::chat) async fn service(
    fabric: &MemoryFabric,
    binding: &ChatTurnBinding,
) -> Arc<crate::runtime::turn_publication::TurnRuntimeService> {
    let config = &binding.generation.effective;
    crate::runtime::turn_publication::TurnPublication {
        generation: Arc::clone(&binding.generation),
        registry: Arc::new(Vec::new()),
        mcp_tool: None,
        security: Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        )),
        observer: Arc::new(crate::observability::noop::NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(config.workspace_dir.clone())),
        memory: fabric.memory(),
        plugin_generation: 0,
    }
    .publish(crate::tools::capability::turn_state::TurnSurfaceKind::Interactive)
    .await
    .unwrap()
}

#[tokio::test]
async fn h2_approval_ledger_requires_h2_origin() {
    let (_dir, fabric, binding, origin) = crate::runtime::h2_entry::recorded_terminal_approval_fixture().await;
    assert!(binding.h2_approval_ledger().is_none(), "no decision or origin");
    let service = service(&fabric, &binding).await;
    assert!(service.execution_approval_ledger().is_some(), "durable fixture ledger");
    binding
        .issue_decision(&service, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    assert!(
        binding.h2_approval_ledger().is_none(),
        "decision without h2 origin must retain interactive approval"
    );
    binding.h2_origin.set(origin).unwrap();
    assert!(
        binding.h2_approval_ledger().is_some(),
        "h2 origin plus decision opts in"
    );
    let (_other_dir, _other_fabric, other, other_origin) =
        crate::runtime::h2_entry::recorded_terminal_approval_fixture().await;
    other.h2_origin.set(other_origin).unwrap();
    assert!(
        other.h2_approval_ledger().is_none(),
        "h2 origin without decision cannot opt in"
    );
    service
        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;
}

#[tokio::test]
async fn guarded_plan_admission_preserves_mode_and_owned_capture() {
    use crate::agent::turn_contracts::ChatMode;
    use crate::runtime::turn_source::{OwnedTurnSourceProof, TurnSourceProof};
    {
        let (_dir, fabric, original) = fixture(true).await;
        let provider =
            ChatProviderBinding::capture(&original.generation, &original.provider, "ollama", "prx-local-model")
                .unwrap()
                .unwrap();
        let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let envelope = super::super::chat_runtime_envelope(fabric.workspace_id(), "prx_plan_session")
            .with_config_generation(&original.generation)
            .with_run_id("prx_plan_run");
        let (_, binding, guard) = provider
            .record_guarded(
                fabric.clone(),
                (
                    envelope.message_scope(),
                    "prx_plan_session",
                    ChatAdmissionMode::Model(ChatMode::Plan),
                    false,
                ),
                "prx plan input",
                "prx_plan_input".to_owned(),
                owner.register(cancellation.clone()).unwrap(),
            )
            .await
            .unwrap();
        guard.handoff();
        assert_eq!(binding.admitted_model_mode().unwrap(), ChatMode::Plan);
        assert!(binding.verify_execution_mode(ChatMode::Plan).is_ok());
        assert!(binding.verify_execution_mode(ChatMode::Edit).is_err());
        assert!(binding.verify_execution_mode(ChatMode::Auto).is_err());
        let queued = Arc::clone(&binding);
        let owned = OwnedTurnSourceProof::capture(queued.as_ref());
        let copied = OwnedTurnSourceProof::capture(&owned);
        assert!(owned.intercepts_writes() && copied.intercepts_writes());
        let service = service(&fabric, &binding).await;
        let context = binding.issue_decision(&service, &cancellation).await.unwrap();
        assert!(
            context
                .runtime()
                .trusted
                .constraints
                .iter()
                .any(|code| code.as_str() == "intercepts_writes")
        );
        let actual = &crate::runtime::decision_advisory::build_state(context.runtime());
        let mut without_plan = actual.clone();
        let mut trusted = without_plan.trusted.parts().clone();
        trusted.constraints.retain(|code| code.as_str() != "intercepts_writes");
        without_plan.trusted = crate::tools::capability::turn_state::TrustedState::from_runtime(trusted);
        let key = crate::decision_engine::state::fixtures::key();
        assert_ne!(
            actual
                .material_fingerprint(&key)
                .expect("test: canonical state serializes"),
            without_plan
                .material_fingerprint(&key)
                .expect("test: canonical state serializes")
        );
        let source = ChatSourceReceipt {
            envelope: binding.envelope.clone(),
            routing_input: Arc::clone(&binding.routing_input),
        };
        let changed_mode = source
            .bind_provider(&provider, ChatAdmissionMode::Model(ChatMode::Edit))
            .unwrap();
        let trusted = actual.trusted.parts();
        assert!(
            crate::runtime::turn_context::validate_agent_retrieval_provenance(
                binding.as_ref(),
                trusted,
                &trusted.resolved_model,
                &trusted.policy_version,
                &trusted.exclusions,
                trusted.model_admission.as_deref(),
            )
            .is_ok()
        );
        assert!(
            crate::runtime::turn_context::validate_agent_retrieval_provenance(
                changed_mode.as_ref(),
                trusted,
                &trusted.resolved_model,
                &trusted.policy_version,
                &trusted.exclusions,
                trusted.model_admission.as_deref(),
            )
            .is_err()
        );
        assert!(queued.verify_execution_mode(ChatMode::Edit).is_err());
        assert!(binding.intercepts_writes());
        service
            .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
            .await;
    }
}

#[tokio::test]
async fn operator_command_binding_cannot_be_reused_as_model_admission() {
    use crate::agent::turn_contracts::ChatMode;
    let (_dir, fabric, original) = fixture(true).await;
    let provider = ChatProviderBinding::capture(&original.generation, &original.provider, "ollama", "prx-local-model")
        .unwrap()
        .unwrap();
    let source = ChatSourceReceipt {
        envelope: original.envelope.clone(),
        routing_input: Arc::clone(&original.routing_input),
    };
    let operator = source
        .bind_provider(&provider, ChatAdmissionMode::OperatorCommand)
        .unwrap();
    for mode in [ChatMode::Plan, ChatMode::Edit, ChatMode::Auto] {
        assert!(operator.verify_execution_mode(mode).is_err());
    }
    assert!(operator.admitted_model_mode().is_err());
    let service = service(&fabric, &operator).await;
    assert!(
        operator
            .issue_decision(&service, &tokio_util::sync::CancellationToken::new())
            .await
            .is_err()
    );
    assert!(!operator.is_decided());
    service
        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;
}

/// Actual durable Chat issuance for exact-call approval lifecycle tests.
/// Mode is captured at ingress; the returned envelope comes from that binding.
pub(crate) async fn recorded_approval_source(
    generation: &Arc<ConfigGeneration>,
    memory: Arc<dyn Memory>,
    input: &str,
    mode: crate::agent::turn_contracts::ChatMode,
    run: &str,
    turn: u64,
) -> (Arc<ChatTurnBinding>, RuntimeEnvelope) {
    let fabric = MemoryFabric::new(memory.clone(), generation.effective.workspace_dir.to_string_lossy())
        .with_event_recording(generation.effective.memory.event_recording_config());
    let session = super::super::session::ChatSession::new("ollama", "prx-local-model");
    let event = super::super::record_chat_user_message_event_in_generation(
        &fabric,
        &session,
        "prx_plan_discovery_session",
        run,
        "ollama",
        "prx-local-model",
        turn,
        input,
        Some(generation),
    )
    .await
    .unwrap();
    let receipt = ChatSourceReceipt::issue(
        memory.as_ref(),
        generation,
        "prx_plan_discovery_session",
        run,
        input,
        &event,
    )
    .await
    .unwrap();
    let provider: Arc<dyn crate::providers::Provider> =
        Arc::new(crate::providers::ollama::OllamaProvider::new(None, None));
    let provider = ChatProviderBinding::capture(generation, &provider, "ollama", "prx-local-model")
        .unwrap()
        .unwrap();
    let binding = receipt
        .bind_provider(&provider, ChatAdmissionMode::Model(mode))
        .unwrap();
    let envelope = binding.envelope().clone();
    (binding, envelope)
}

pub(in crate::chat) fn install_fixture_terminal(
    binding: &ChatTurnBinding,
    owner: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
    fabric: MemoryFabric,
) {
    binding
        .install_terminal(
            owner.register(tokio_util::sync::CancellationToken::new()).unwrap(),
            fabric,
        )
        .unwrap();
}

#[tokio::test]
async fn closed_input_cannot_issue_a_decision() {
    let (_dir, fabric, binding) = fixture(true).await;
    let service = service(&fabric, &binding).await;
    binding
        .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Failed, "failed")
        .await
        .unwrap();
    let result = binding
        .issue_decision(&service, &tokio_util::sync::CancellationToken::new())
        .await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("closed before decision issuance")
    );
    assert!(!binding.is_decided());
    service
        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;
}

#[tokio::test]
async fn decided_input_rejects_admission_failure_and_commits_redacted_reply() {
    let (dir, fabric, binding) = fixture(true).await;
    let service = service(&fabric, &binding).await;
    binding
        .issue_decision(&service, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    assert!(
        binding
            .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Failed, "failed",)
            .await
            .is_err()
    );
    assert!(
        fabric
            .memory()
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed",)
            .await
            .unwrap()
            .is_none()
    );
    let route = crate::llm::route_decision::RouteDecision::single_candidate("ollama", "prx-local-model");
    let reply = format!("prx reply {}", crate::agent::terminal::persistence_probe::PROBE_SECRET);
    let receipt = super::super::finalize_chat_success_turn(
        Some(binding.as_ref()),
        &fabric,
        "prx_session",
        "prx_run",
        "ollama",
        "prx-local-model",
        binding.envelope().message_scope(),
        crate::llm::route_decision::ProviderExecutionOutcome::success_for_decision(&route, chrono::Utc::now()),
        Some(reply),
        1,
        "completed",
        &binding.generation.effective.cost,
        dir.path(),
    )
    .await
    .unwrap();
    assert!(receipt.assistant_event.unwrap().content.contains("[REDACTED]"));
    assert!(receipt.terminal_event.id > 0);
    crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(dir.path());
    service
        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;
}

#[tokio::test]
async fn admission_failure_retries_redact_and_reject_conflicting_status() {
    let (_dir, fabric, binding) = fixture(true).await;
    let summary = format!("failed {}", crate::agent::terminal::persistence_probe::PROBE_SECRET);
    for _ in 0..2 {
        binding
            .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Failed, &summary)
            .await
            .unwrap();
    }
    let memory = fabric.memory();
    let event = memory
        .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
        .await
        .unwrap()
        .unwrap();
    assert!(event.id > 0);
    assert!(
        !event
            .raw_payload_json
            .as_deref()
            .unwrap()
            .contains(crate::agent::terminal::persistence_probe::PROBE_SECRET,)
    );
    assert!(
        binding
            .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Cancelled, &summary,)
            .await
            .is_err()
    );
    assert!(
        memory
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:final",)
            .await
            .unwrap()
            .is_none()
    );
    assert!(*binding.admission_closed.lock().await);
    assert!(!binding.is_decided());
}

#[tokio::test]
async fn admission_failure_rejects_a_backend_without_the_original_input() {
    let (_dir, fabric, binding) = fixture(true).await;
    let foreign = tempfile::tempdir().unwrap();
    let foreign_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(foreign.path()).unwrap());
    let foreign_fabric = MemoryFabric::new(foreign_memory.clone(), fabric.workspace_id());
    assert!(
        binding
            .record_predecision_failure(&foreign_fabric, "prx_run", TurnTerminalStatus::Failed, "failed",)
            .await
            .is_err()
    );
    assert!(
        foreign_memory
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed",)
            .await
            .unwrap()
            .is_none()
    );
    // A failed persistence attempt closes admission but allows a safe retry.
    binding
        .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Failed, "failed")
        .await
        .unwrap();
}

#[tokio::test]
async fn admission_failure_respects_disabled_user_recording() {
    let (_dir, fabric, binding) = fixture(false).await;
    binding
        .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Cancelled, "cancelled")
        .await
        .unwrap();
    assert!(*binding.admission_closed.lock().await);
    assert!(
        fabric
            .memory()
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed",)
            .await
            .unwrap()
            .is_none()
    );
}
