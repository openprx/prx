#![cfg(test)]
//! Three original worker probes obtain a live parent-issued, unused manifest.
//! The parent Delegate backend is controlled; the child uses physical loopback IO.
use super::*;

pub(crate) async fn with_manifest<T, F, Fut>(
    config: crate::config::Config,
    mut manifest: crate::session_worker::protocol::WorkerManifest,
    consume: F,
) -> anyhow::Result<T>
where
    F: FnOnce(crate::session_worker::protocol::WorkerManifest) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    struct ParentConfigScope {
        previous: Option<std::path::PathBuf>,
        _lock: parking_lot::MutexGuard<'static, ()>,
    }
    impl Drop for ParentConfigScope {
        fn drop(&mut self) {
            crate::config::schema::swap_process_config_dir_for_tests(self.previous.take());
        }
    }
    let config_lock = crate::config::schema::process_config_dir_test_lock();
    let _config_scope = ParentConfigScope {
        previous: crate::config::schema::swap_process_config_dir_for_tests(Some(manifest.config_dir.clone())),
        _lock: config_lock,
    };
    let workspace = config.workspace_dir.clone();
    let owned_runs = Arc::new(tokio::sync::RwLock::new(Vec::<SubAgentRun>::new()));
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())?;
    let outcome = cleanup
        .run(boxed_parent_issuance(|| async {
            Ok(std::panic::AssertUnwindSafe(boxed_parent_issuance(|| async {
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
                let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(spawn)]);
                assert!(tools_handle.set(&registry));
                let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
                    Arc::clone(&registry),
                    Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
                    Arc::new(DenyApprovalStrategy),
                    Arc::new(AdapterOwnedPreparation),
                    Arc::new(TracingToolExecutionAudit),
                ));
                let catalog = ProviderCatalogView::from_config(&config);
                let transports = crate::decision::compatible_http::HttpDecisionTransports;
                let mut service = build_turn_decision_service(
                    TurnSurfaceKind::AgentLoop,
                    TurnServiceBuild {
                        parent_grant: None,
                        generation: Arc::clone(&generation),
                        registry,
                        execution,
                        catalog_sources: sources(&generation, &config),
                        surface_policy: SurfacePolicy {
                            direct_names: BTreeSet::from(["sessions_spawn".into()]),
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
                    "sessions_spawn: Describe the project.",
                    "prx_parent_process_component",
                )
                .await;
                let cancellation = CancellationToken::new();
                let context = service
                    .decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation)
                    .await?;
                assert!(matches!(context.directive(), TurnExecutionDirective::Delegate { .. }));
                assert!(context.record_admitted.load(Ordering::Acquire));
                let selected = service.agent_execution_specs(&proof, &context)?;
                assert_eq!(selected.specs.len(), 1);
                assert_eq!(
                    selected.specs.first().expect("one selected spawn spec").name,
                    "sessions_spawn"
                );
                let envelope = RuntimeEnvelope::agent(proof.workspace_id(), proof.run_id())
                    .with_config_generation(&generation)
                    .with_source_message_event_id(proof.source_event_id().expect("real parent source"));
                let original_execution =
                    ToolExecutionContext::new(envelope.clone(), "private").with_chat_id(proof.session_key());
                let (_execution, bound) =
                    service.bind_agent_execution(&proof, &context, &original_execution, &selected.specs)?;
                let grant = bound.child_grant.as_ref().expect("actual parent-issued grant");

                let selection = crate::session_worker::protocol::WorkerProviderSelection {
                    provider: manifest.provider_name.clone(),
                    model: manifest.model.clone(),
                    credential: crate::providers::selection::credential_for(&config, &manifest.provider_name),
                };
                // Resolve the same real PMM child selection before issuing the manifest.
                let binding = crate::runtime::child_source::ChildProviderBinding::build(
                    Arc::clone(grant),
                    &selection.provider,
                    &selection.model,
                    selection.credential.clone(),
                )?;
                let run_id = uuid::Uuid::new_v4().to_string();
                let session = format!("{}:child:{run_id}", grant.parent().session_key());
                let mut scope =
                    crate::memory::MessageEventScope::new("sessions_spawn", crate::memory::MemoryVisibility::Workspace)
                        .with_run_id(&run_id)
                        .with_session_key(&session);
                grant.bind_event_scope(&mut scope)?;
                let receipt = crate::runtime::child_source::record_child_input(
                    grant.source_fabric(),
                    scope,
                    &manifest.task,
                    Some(format!("{run_id}:input")),
                    None,
                    Some(grant),
                )
                .await?
                .expect("actual persisted child input");
                let event = receipt.event();
                manifest.parent_capability = None;
                manifest.run_id = run_id;
                manifest.session_scope_key = session;
                manifest.owner_id = event.owner_id.clone();
                manifest.parent_run_id = event.parent_run_id.clone();
                manifest.runtime_config_generation_id = event.config_generation_id;
                manifest.runtime_config_source_revision = event.config_source_revision.clone();
                manifest.scope_channel = event.channel.clone();
                manifest.scope_sender = event.sender.clone();
                manifest.scope_chat_type = Some(grant.parent().policy_chat_type().to_owned());
                manifest.scope_chat_id = Some(grant.parent().session_key().to_owned());
                manifest.agent_id = event.agent_id.clone();
                manifest.parent_task_id = Some(grant.parent().run_id().to_owned());
                manifest.source_message_event_id = event.causation_event_id.clone();
                manifest.config_generation =
                    crate::session_worker::protocol::config_source_generation(&manifest.config_dir)?;
                grant.bind_worker_manifest(&mut manifest, &receipt, &selection)?;
                // Keep the actual parent decision owner and PMM binding alive while the
                // caller consumes this one unused manifest. No child pre-execution/replay.
                let result = boxed_worker_probe(consume, manifest).await;
                drop(binding);
                drop(bound);
                drop(context);
                drop(service);
                result
            }))
            .catch_unwind()
            .await)
        }))
        .await?;
    // The work future and its issuer are dropped before CleanupScope drains.
    match outcome {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

// Construct the existing boxed parent work before the cleanup owner polls it.
// The closure returns a lazy future; all issuance and catch logic stays inside.
#[inline(never)]
fn boxed_parent_issuance<F, Fut>(make_work: F) -> std::pin::Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    Box::pin(make_work())
}

// Construct the callback future in this separate frame before the parent
// catch/cleanup state polls it. The issuer remains live until the await ends.
#[inline(never)]
fn boxed_worker_probe<'a, T, F, Fut>(
    consume: F,
    manifest: crate::session_worker::protocol::WorkerManifest,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>> + 'a>>
where
    F: FnOnce(crate::session_worker::protocol::WorkerManifest) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>> + 'a,
{
    Box::pin(consume(manifest))
}
