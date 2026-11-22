#![cfg(test)]

//! Restart regressions use the real SQLite memory.
use super::*;
use crate::agent::terminal::TurnTerminalReceipt;
use crate::chat::binding::{ChatAdmissionMode, ChatProviderBinding, ChatTurnBinding};
use crate::memory::{MessageEvent, MessageEventInput, SqliteMemory};
use crate::runtime::turn_cleanup::{CleanupScope, CleanupTracker};
use crate::util::test_helpers::run_in_isolated_process;
use anyhow::Context;
use std::path::Path;

fn config_at(workspace: &Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.config_path = workspace.join("config.toml");
    config.default_provider = Some("ollama".into());
    config.default_model = Some("prx-local-model".into());
    config.cost.prices.insert(
        "ollama/prx-local-model".into(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    // Recovery fixtures persist a real completed provider turn.
    config.decision_engine.backend.get_or_insert_with(Default::default).kind =
        crate::config::schema::decision_engine::BackendSelection::Fixed(
            crate::config::schema::decision_engine::BackendKind::Builtin,
        );
    config
}

async fn publish(
    memory: Arc<dyn Memory>,
    generation: Arc<crate::config::ConfigGeneration>,
) -> Result<Arc<crate::runtime::turn_publication::TurnRuntimeService>> {
    crate::runtime::turn_publication::TurnPublication {
        security: Arc::new(crate::security::SecurityPolicy::from_config(
            &generation.effective.autonomy,
            &generation.effective.workspace_dir,
        )),
        hooks: Arc::new(crate::hooks::HookManager::new(
            generation.effective.workspace_dir.clone(),
        )),
        generation,
        registry: Arc::new(Vec::new()),
        mcp_tool: None,
        observer: Arc::new(crate::observability::noop::NoopObserver),
        memory,
        plugin_generation: 0,
    }
    .publish(crate::tools::capability::turn_state::TurnSurfaceKind::Interactive)
    .await
}

async fn guarded_input(
    memory: Arc<dyn Memory>,
    generation: &Arc<crate::config::ConfigGeneration>,
    run: &str,
) -> Result<(
    MessageEvent,
    Arc<crate::chat::binding::ChatTurnBinding>,
    crate::chat::binding::ChatAdmissionGuard,
)> {
    let provider: Arc<dyn Provider> = Arc::new(crate::providers::ollama::OllamaProvider::new(None, None));
    guarded_input_with(provider, memory, generation, run).await
}

async fn guarded_input_with(
    provider: Arc<dyn Provider>,
    memory: Arc<dyn Memory>,
    generation: &Arc<crate::config::ConfigGeneration>,
    run: &str,
) -> Result<(
    MessageEvent,
    Arc<crate::chat::binding::ChatTurnBinding>,
    crate::chat::binding::ChatAdmissionGuard,
)> {
    let selected = ChatProviderBinding::capture(generation, &provider, "ollama", "prx-local-model")?
        .context("selected provider")?;
    let fabric = MemoryFabric::new(memory, generation.effective.workspace_dir.to_string_lossy())
        .with_event_recording(generation.effective.memory.event_recording_config());
    let envelope = chat_runtime_envelope(fabric.workspace_id(), "prx-restart-session")
        .with_config_generation(generation)
        .with_run_id(run);
    let owner = crate::runtime::turn_cleanup::current_tracker().unwrap_or_else(|| Arc::new(CleanupTracker::default()));
    let (source, binding, guard) = selected
        .record_guarded_with_origin(
            fabric,
            (
                envelope.message_scope(),
                "prx-restart-session",
                ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
                false,
            ),
            "  ",
            format!("chat:{run}:input"),
            owner.register(CancellationToken::new())?,
            None,
        )
        .await?;
    Ok((source.context("durable source")?, binding, guard))
}

/// Counts every provider dispatch; admission must never reach one.
#[derive(Default)]
struct DispatchCounter(std::sync::atomic::AtomicUsize);

struct CountingProvider(Arc<DispatchCounter>);

#[async_trait::async_trait]
impl Provider for CountingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        self.0.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("dispatch fixture")
    }
}

async fn completed_chat(workspace: &Path) -> MessageEvent {
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    let lifetime_owner = cleanup
        .scope_admission(async {
            Ok(Arc::downgrade(
                &crate::runtime::turn_cleanup::current_tracker().expect("test: cleanup owner installed"),
            ))
        })
        .await
        .unwrap();
    let source = completed_chat_under_cleanup(workspace, &cleanup).await;
    cleanup
        .run(async { Ok(()) })
        .await
        .expect("test: drain completed ingress lifetime before restart");
    assert!(
        lifetime_owner.upgrade().is_none(),
        "test: completed ingress lifetime owner must be released before restart"
    );
    source
}

/// Journal-backed close does not await the auxiliary decision-store queue.
/// A fixture that deliberately selects legacy store replay needs its own
/// durability acknowledgement before asserting that replay can verify it.
async fn persist_completed_chat(
    binding: &ChatTurnBinding,
    fabric: &MemoryFabric,
    workspace: &Path,
    history_commit_len: usize,
) -> TurnTerminalReceipt {
    use crate::agent::terminal::{
        TurnDeliveryIntent, TurnHistoryProjection, TurnTerminalDraft, TurnTerminalStatus, TurnTerminalTelemetry,
    };
    use crate::agent::turn_request::{ToolLoopMemory, TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
    struct FixtureProvider;
    #[async_trait::async_trait]
    impl Provider for FixtureProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> Result<String> {
            Ok("Project response".into())
        }
    }
    let routing = binding.decision_routing().expect("test: admitted source");
    let memory = fabric.memory();
    let registry: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(Vec::new());
    let observer = crate::observability::NoopObserver;
    let hooks = crate::hooks::HookManager::new(workspace.to_owned());
    let cancellation = CancellationToken::new();
    let started = chrono::Utc::now();
    let mut history = vec![
        crate::providers::ChatMessage::system("Project assistant"),
        crate::providers::ChatMessage::user("  "),
    ];
    let (_, trace) = Box::pin(crate::agent::turn_engine::run_turn(
        &mut history,
        TurnRequest {
            advice: None,
            channel_name: "terminal",
            scope: None,
            mode: Default::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: TurnSchedule {
                read_only_tool_concurrency_window: 1,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: routing,
        },
        TurnServices {
            provider: &FixtureProvider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry: registry,
            observer: &observer,
            hooks: &hooks,
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
                live_frames: None,
                guarded_delivery: false,
                provider_trace: binding.retained_provider_trace(),
                events: None,
                stream_provider: false,
                tool_execution_service: None,
                tool_execution_context: crate::tools::ToolExecutionContext::new(binding.envelope().clone(), "private"),
                allowed_tool_names: None,
            }),
            memory: ToolLoopMemory::new(&memory, workspace, None),
        },
        TurnDelivery {
            cancellation: Some(cancellation),
            on_delta: None,
            on_tool_call: None,
        },
    ))
    .await
    .expect("test: real recovery fixture turn");
    binding.capture_trace(&trace).expect("test: authenticated trace");
    let route = crate::llm::route_decision::RouteDecision::single_candidate("ollama", "prx-local-model");
    let provider = crate::agent::terminal::provider_outcome_from_trace(&route, started, trace);
    let run = binding.envelope().run_id.as_deref().expect("test: completed run");
    let scope = binding.envelope().message_scope();
    binding
        .commit_terminal(
            fabric,
            TurnTerminalDraft {
                binding: binding.terminal_binding(run).expect("test: terminal binding"),
                terminal_id: run.to_owned(),
                scope: scope.clone(),
                status: TurnTerminalStatus::Completed,
                history: Some(TurnHistoryProjection {
                    assistant_content: "Project response".into(),
                    history_commit_len,
                }),
                history_scope: Some(scope.with_sender("prx_runtime").with_recipient("local-user")),
                provider_outcome: Some(provider),
                telemetry: TurnTerminalTelemetry {
                    summary: "Project response".into(),
                    started_at: started,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: TurnDeliveryIntent::Reply {
                    target: "terminal:local-user".into(),
                },
            },
            &config_at(workspace).cost,
            workspace,
        )
        .await
        .expect("test: persist provider terminal")
}

async fn completed_chat_under_cleanup(workspace: &Path, cleanup: &CleanupScope) -> MessageEvent {
    cleanup
        .scope_admission(async {
            let memory = Arc::new(SqliteMemory::new(workspace).unwrap());
            let generation = crate::config::new_shared(config_at(workspace)).pin();
            let (source, binding, guard) = guarded_input(memory.clone(), &generation, "prx-first-run")
                .await
                .unwrap();
            let service = publish(memory.clone(), generation).await.unwrap();
            let cancel = CancellationToken::new();
            binding.issue_decision(&service, &cancel).await.unwrap();
            let fabric = MemoryFabric::new(memory.clone(), workspace.to_string_lossy());
            let _receipt = persist_completed_chat(&binding, &fabric, workspace, 2).await;
            guard.handoff();
            service
                .shutdown(std::time::Instant::now() + Duration::from_secs(5))
                .await;
            Ok(source)
        })
        .await
        .unwrap()
}

async fn restarted_chat(workspace: &Path, run: &str) -> MessageEvent {
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    cleanup
        .run(async {
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).unwrap());
            let generation = crate::config::new_shared(config_at(workspace)).pin();
            let (source, _binding, guard) = guarded_input(memory.clone(), &generation, run).await.unwrap();
            let service = publish(memory, generation).await.unwrap();
            guard.handoff();
            service
                .shutdown(std::time::Instant::now() + Duration::from_secs(5))
                .await;
            Ok(source)
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn chat_drained_service_releases_ingress_lease_before_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    let (first, lease_token) = cleanup
        .scope_admission(async {
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
            let generation = crate::config::new_shared(config_at(dir.path())).pin();
            let (source, binding, guard) = guarded_input(memory.clone(), &generation, "prx-drained-run").await?;
            let lease = memory
                .acquire_ingress_lifetime(
                    &source.workspace_id,
                    &uuid::Uuid::new_v4().to_string(),
                    std::time::Instant::now() + Duration::from_secs(5),
                )
                .await?;
            let token = lease.token().to_owned();
            let service = publish(memory.clone(), generation).await?;
            let cancel = CancellationToken::new();
            binding.issue_decision(&service, &cancel).await?;
            let fabric = MemoryFabric::new(memory, dir.path().to_string_lossy());
            let _ = persist_completed_chat(&binding, &fabric, dir.path(), 2).await;
            guard.handoff();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            assert!(service.drain_writes(deadline).await, "first service writes must drain");
            Ok((source, token))
        })
        .await
        .unwrap();
    cleanup.run(async { Ok(()) }).await.unwrap();
    let probe = SqliteMemory::new(dir.path()).unwrap();
    let claim = probe
        .claim_ingress_recovery(
            &first.workspace_id,
            &lease_token,
            std::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(claim.is_some(), "a drained service must release the ingress lease");
    drop(claim);
    let next = restarted_chat(dir.path(), "prx-after-drain").await;
    assert!(next.id > first.id);
}

#[tokio::test]
async fn retry_after_active_key_loss_is_refused_at_admission() {
    let dir = tempfile::tempdir().unwrap();
    let _first = completed_chat(dir.path()).await;
    // Corrupt the active recovery key: preparation now yields no recorder.
    let key_id = SqliteMemory::new(dir.path())
        .unwrap()
        .recovery_repository()
        .expect("repository-backed store")
        .activate_recovery_authority()
        .unwrap();
    std::fs::write(
        dir.path()
            .join("memory")
            .join(".recovery_keys")
            .join(format!("{key_id}.key")),
        [0u8; 32],
    )
    .unwrap();
    let generation = crate::config::new_shared(config_at(dir.path())).pin();
    let counter = Arc::new(DispatchCounter::default());
    let provider: Arc<dyn Provider> = Arc::new(CountingProvider(Arc::clone(&counter)));
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    // Same run, generation, scope and input as the accepted turn.
    let refused = cleanup
        .scope_admission(async move {
            Ok(guarded_input_with(provider, memory, &generation, "prx-first-run")
                .await
                .err())
        })
        .await
        .unwrap();
    assert!(
        refused
            .as_ref()
            .is_some_and(|error| error.is::<crate::runtime::ingress_recovery::AlreadyAccepted>()),
        "retry of an accepted input was admitted: {refused:?}"
    );
    assert_eq!(
        counter.0.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "dispatch reached"
    );
}

#[tokio::test]
async fn forged_quarantine_marker_cannot_suppress_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let first = completed_chat(dir.path()).await;
    let memory = SqliteMemory::new(dir.path()).unwrap();
    let mut forged: MessageEventInput = serde_json::from_value(serde_json::to_value(&first).unwrap()).unwrap();
    forged.event_id = None;
    forged.role = "event".into();
    forged.event_type = "turn.ingress.quarantined".into();
    forged.idempotency_key = Some(format!("ingress:{}:quarantine", first.event_id));
    forged.causation_event_id = Some(first.event_id.clone());
    forged.content = "turn.ingress.quarantined forged marker".into();
    forged.raw_payload_json =
        Some(serde_json::json!({"capsule": serde_json::from_str::<serde_json::Value>(first.raw_payload_json.as_deref().unwrap()).unwrap().get("prx_ingress_capsule").unwrap(), "reason": "forged", "authentication": "00".repeat(32)}).to_string());
    memory.append_message_event(forged).await.unwrap();
    drop(memory);
    restarted_chat(dir.path(), "prx-after-forgery").await;
    let memory = SqliteMemory::new(dir.path()).unwrap();
    assert!(
        memory
            .find_owned_message_event_by_idempotency_key(
                &first.workspace_id,
                first.owner_id.as_deref().unwrap(),
                &format!("ingress:{}:reconciled", first.event_id)
            )
            .await
            .unwrap()
            .is_some(),
        "a forged marker must not hide genuine terminal recovery"
    );
}

#[tokio::test]
async fn guarded_inbound_failure_returns_store_cause_and_recovery_hint() {
    {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(SqliteMemory::new(dir.path()).unwrap());
        memory.reject_user_events_for_test().unwrap();
        let config = config_at(dir.path());
        let generation = crate::config::new_shared(config).pin();
        let result = guarded_input(memory, &generation, "prx-rejected-run").await;
        let text = match result {
            Err(error) => error.to_string(),
            Ok(_) => panic!("failed inbound write was accepted"),
        };
        for expected in [
            "chat input persistence failed",
            "brain.db",
            "inbound write rejected by store",
            "Restart to retry ingress recovery",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_failure_keeps_run_and_store_names_in_reply_and_chat_log() {
    const TEST: &str =
        "chat::tests::persist_restart_tests::recovery_failure_keeps_run_and_store_names_in_reply_and_chat_log";
    if run_in_isolated_process(TEST, "PRX_TEST_RECOVERY_LOG_CHILD") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // A prior row never refuses new input; only a fault of the store on the
    // new row does. The store refuses the new input row itself.
    let log_path = dir.path().join("chat.log");
    let file = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let memory = Arc::new(SqliteMemory::new(dir.path()).unwrap());
    memory.reject_user_events_for_test().unwrap();
    let generation = crate::config::new_shared(config_at(dir.path())).pin();
    let run = "prx-current-run";
    let error = match guarded_input(memory, &generation, run).await {
        Err(error) => error,
        Ok(_) => panic!("a store fault on the new row must refuse the input"),
    };
    let reply = error.to_string();
    let log = std::fs::read_to_string(&log_path).unwrap();
    let line = log
        .lines()
        .find(|line| line.contains("chat input persistence failed"))
        .unwrap();
    for expected in [
        run,
        "brain.db",
        "inbound write rejected by store",
        "Restart to retry ingress recovery",
    ] {
        assert!(reply.contains(expected), "missing {expected}: {reply}");
    }
    // The run id reaches the log as its class (length and digest), never as text.
    let run_class = format!("run_id=kind=text len={}", run.len());
    let run_digest = crate::agent::diagnostics::text_class(run).to_string();
    for expected in [run_class.as_str(), run_digest.as_str(), "brain.db", "sha256="] {
        assert!(
            line.contains(expected),
            "the log line classifies the run, names the store and classifies the error: missing {expected}: {line}"
        );
    }
    assert!(
        !line.contains(run),
        "the log line never carries the run id text: {line}"
    );
    assert!(!reply.contains('/'), "full paths must stay out of the reply: {reply}");
    let store = dir.path().join("memory/brain.db");
    assert!(
        !line.contains(store.to_str().unwrap()) && !line.contains("inbound write rejected by store"),
        "the log line carries the error's class, never its text or the store path: {line}"
    );
    assert!(line.contains("run_id="), "missing structured run field: {line}");

    let private_error = anyhow::anyhow!("permission denied")
        .context("https://prx_user:prx_fixture_password@example.com/v1?key=prx_fixture_query")
        .context(crate::runtime::ingress_recovery::IngressRecoveryDiagnostic {
            run_id: "/prx-private-run".into(),
            memory_store: dir.path().join("memory/brain.db"),
        });
    let private_reply = crate::chat::binding::chat_input_persistence_failure(&private_error).to_string();
    assert!(!private_reply.contains('/'), "{private_reply}");
    assert!(!private_reply.contains("prx-private-run"), "{private_reply}");
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(!log.contains("prx_fixture_password"), "credential reached chat.log");
    assert!(!log.contains("prx_fixture_query"), "URL query secret reached chat.log");
    assert!(
        !log.contains("prx-private-run"),
        "the private run id reached chat.log: {log}"
    );
    let redacted_run = crate::agent::diagnostics::text_class("[redacted]").to_string();
    assert!(
        log.contains(&redacted_run),
        "a private run id is redacted before its class is logged: {log}"
    );
}

/// The reconciled evidence the owner wrote for `source`, and proof that no
/// row of its run is left unsettled (I3).
async fn assert_owner_settled(workspace: &Path, source: &MessageEvent, cell: &str) {
    let memory = SqliteMemory::new(workspace).unwrap();
    let owner = source.owner_id.as_deref().expect("test: input owner");
    let evidence = memory
        .find_owned_message_event_by_idempotency_key(
            &source.workspace_id,
            owner,
            &format!("ingress:{}:reconciled", source.event_id),
        )
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("test: {cell}: the owner settles its own ingress row"));
    assert_eq!(
        evidence.content, "turn.ingress.reconciled final committed by its owner",
        "{cell}"
    );
    let run = source.run_id.as_deref().expect("test: input run");
    assert!(
        memory
            .find_owned_message_event_by_idempotency_key(&source.workspace_id, owner, &format!("turn:{run}:final"))
            .await
            .unwrap()
            .is_some(),
        "{cell}: the settlement commits with the run's final"
    );
    let inspection = crate::memory::sqlite::inspect_unsettled_ingress(&workspace.join("memory/brain.db")).unwrap();
    assert!(
        inspection.rows.iter().all(|row| row.run_id != run),
        "{cell}: no unsettled row of the run remains: {:?}",
        inspection.rows
    );
}

/// One recorded chat input ended by a terminal of `status`, committed
/// through the real chat binding. `refused` ends it as a policy refusal does:
/// failed, with no provider outcome and a suppressed delivery.
async fn chat_terminal_cell(
    workspace: &Path,
    run: &str,
    status: crate::agent::terminal::TurnTerminalStatus,
    refused: bool,
) -> MessageEvent {
    use crate::agent::terminal::{TurnDeliveryIntent, TurnTerminalDraft, TurnTerminalTelemetry};
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    let source = cleanup
        .scope_admission(async {
            let memory = Arc::new(SqliteMemory::new(workspace).unwrap());
            let generation = crate::config::new_shared(config_at(workspace)).pin();
            let (source, binding, guard) = guarded_input(memory.clone(), &generation, run).await?;
            let service = publish(memory.clone(), generation).await?;
            binding.issue_decision(&service, &CancellationToken::new()).await?;
            let fabric = MemoryFabric::new(memory, workspace.to_string_lossy());
            let scope = binding.envelope().message_scope();
            let now = chrono::Utc::now();
            binding
                .commit_terminal(
                    &fabric,
                    TurnTerminalDraft {
                        binding: binding.terminal_binding(run)?,
                        terminal_id: run.to_owned(),
                        scope,
                        status,
                        history: None,
                        history_scope: None,
                        provider_outcome: None,
                        telemetry: TurnTerminalTelemetry {
                            summary: if refused { "refused by policy" } else { "turn ended" }.into(),
                            started_at: now,
                            finished_at: now,
                        },
                        delivery_intent: if refused {
                            TurnDeliveryIntent::Suppress {
                                reason: "policy refusal".into(),
                            }
                        } else {
                            TurnDeliveryIntent::None
                        },
                    },
                    &config_at(workspace).cost,
                    workspace,
                )
                .await?;
            guard.handoff();
            service
                .shutdown(std::time::Instant::now() + Duration::from_secs(5))
                .await;
            Ok(source)
        })
        .await
        .unwrap();
    cleanup.run(async { Ok(()) }).await.unwrap();
    source
}

/// A provider that answers one embedded one-shot turn.
struct OneShotProvider;

#[async_trait::async_trait]
impl Provider for OneShotProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> Result<String> {
        anyhow::bail!("fixture requires the traced provider dispatch")
    }

    async fn chat_traced(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> Result<crate::providers::traits::ChatTrace> {
        use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage, TokenUsageSource};
        let now = chrono::Utc::now();
        Ok(crate::providers::traits::ChatTrace {
            response: crate::providers::ChatResponse {
                text: Some("One-shot answer.".into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "ollama".into(),
                model: model.into(),
                started_at: now,
                finished_at: now,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "ollama".into(),
            final_model: model.into(),
            served_member: None,
            tokens_used: TokenUsage {
                prompt_tokens: Some(3),
                completion_tokens: Some(2),
                total_tokens: Some(5),
                source: TokenUsageSource::Reported,
                ..TokenUsage::default()
            },
        })
    }
}

/// One embedded single-shot agent turn: input recorded, provider answered,
/// terminal committed by the agent's own terminal owner.
async fn one_shot_cell(workspace: &Path) -> MessageEvent {
    const MODEL: &str = "prx-one-shot-model";
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).unwrap());
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.config_path = workspace.join("config.toml");
    config.default_model = Some(MODEL.into());
    config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"ollama\"\nmodel = \"{MODEL}\"\noperations = [\"chat\"]\nnative_tools = true\n"
        ))
        .unwrap(),
    );
    let mut agent = crate::agent::agent::Agent::builder()
        .runtime_config(config, "ollama")
        .provider(Box::new(OneShotProvider))
        .tools(Vec::new())
        .memory(Arc::clone(&memory))
        .observer(Arc::new(crate::observability::NoopObserver {}))
        .tool_dispatcher(Box::new(crate::agent::dispatcher::XmlToolDispatcher))
        .build()
        .unwrap();
    Box::pin(agent.turn("one-shot question")).await.unwrap();
    drop(agent);
    let workspace_id = workspace.to_string_lossy().into_owned();
    let owner = format!("owner:{workspace_id}:agent_api:embedded-caller");
    let inputs = memory
        .list_owned_message_events_by_type(&workspace_id, &owner, "message.created", 0, 16)
        .await
        .unwrap();
    inputs
        .into_iter()
        .find(|event| event.role == "user")
        .expect("test: the one-shot input is recorded")
}

#[tokio::test]
async fn a_terminal_commit_settles_its_own_ingress_row() {
    use crate::agent::terminal::TurnTerminalStatus;
    let completed = tempfile::tempdir().unwrap();
    let source = completed_chat(completed.path()).await;
    assert_owner_settled(completed.path(), &source, "completed").await;

    for (cell, status, refused) in [
        ("failed", TurnTerminalStatus::Failed, false),
        ("cancelled", TurnTerminalStatus::Cancelled, false),
        ("refused", TurnTerminalStatus::Failed, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let source = chat_terminal_cell(dir.path(), &format!("prx-{cell}-run"), status, refused).await;
        assert_owner_settled(dir.path(), &source, cell).await;
    }

    let one_shot = tempfile::tempdir().unwrap();
    let source = one_shot_cell(one_shot.path()).await;
    assert_owner_settled(one_shot.path(), &source, "one-shot").await;
}

#[tokio::test]
async fn an_orphaned_row_is_upgraded_when_its_final_is_found() {
    let dir = tempfile::tempdir().unwrap();
    // The lock directory moves mid-turn, as a reboot that renumbers the volume
    // does: the owner can no longer prove its lifetime, so its final commits
    // without settling the row, and the row's lock is out of reach.
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    let first = cleanup
        .scope_admission(async {
            let memory = Arc::new(SqliteMemory::new(dir.path()).unwrap());
            let generation = crate::config::new_shared(config_at(dir.path())).pin();
            let (source, binding, guard) = guarded_input(memory.clone(), &generation, "prx-first-run").await?;
            let service = publish(memory.clone(), generation).await?;
            binding.issue_decision(&service, &CancellationToken::new()).await?;
            let root = dir.path().join(".prx-ingress-recovery-v1");
            let locks: Vec<_> = std::fs::read_dir(&root)?
                .take(2)
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<_>>()?;
            assert_eq!(locks.len(), 1, "test: one lock directory");
            for lock in &locks {
                std::fs::rename(lock, lock.with_extension("moved"))?;
            }
            let fabric = MemoryFabric::new(memory.clone(), dir.path().to_string_lossy());
            let _receipt = persist_completed_chat(&binding, &fabric, dir.path(), 2).await;
            guard.handoff();
            service
                .shutdown(std::time::Instant::now() + Duration::from_secs(5))
                .await;
            Ok(source)
        })
        .await
        .unwrap();
    cleanup.run(async { Ok(()) }).await.unwrap();
    let workspace = first.workspace_id.clone();
    let owner = first.owner_id.clone().expect("test: input owner");
    let key = |suffix: &str| format!("ingress:{}:{suffix}", first.event_id);
    {
        let memory = SqliteMemory::new(dir.path()).unwrap();
        assert!(
            memory
                .find_owned_message_event_by_idempotency_key(&workspace, &owner, &key("reconciled"))
                .await
                .unwrap()
                .is_none(),
            "test: the final committed without settling the row"
        );
    }

    // The next process accepts its input (orphaning the row) and its startup
    // recovery finds the authenticated final and upgrades the row.
    restarted_chat(dir.path(), "prx-after-move").await;
    let memory = SqliteMemory::new(dir.path()).unwrap();
    for (suffix, state) in [("unconfirmed", "unconfirmed"), ("reconciled", "reconciled")] {
        let evidence = memory
            .find_owned_message_event_by_idempotency_key(&workspace, &owner, &key(suffix))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("test: {suffix} evidence"));
        let payload: serde_json::Value = serde_json::from_str(evidence.raw_payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            payload
                .pointer("/ingress_recovery/state")
                .and_then(serde_json::Value::as_str),
            Some(state)
        );
        assert_eq!(
            payload
                .pointer("/ingress_recovery/cause")
                .and_then(serde_json::Value::as_str),
            Some("lock_missing")
        );
    }
    let inspection = crate::memory::sqlite::inspect_unsettled_ingress(&dir.path().join("memory/brain.db")).unwrap();
    assert!(
        inspection
            .rows
            .iter()
            .all(|row| Some(row.run_id.as_str()) != first.run_id.as_deref()),
        "the upgraded row is reconciled: {:?}",
        inspection.rows
    );
}
