#![cfg(test)]
use super::*;

#[tokio::test]
async fn enabled_agent_rejects_memory_without_message_events_before_key_or_store() {
    for backend in ["none", "markdown"] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.memory.backend = backend.to_string();
        config.causal_tree.enabled = true;
        let generation = crate::config::new_shared(config.clone()).pin();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let security = Arc::new(SecurityPolicy::default());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let memory: Arc<dyn Memory> = if backend == "none" {
            Arc::new(NoneMemory::new())
        } else {
            Arc::new(crate::memory::MarkdownMemory::new(workspace.path()))
        };
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let result = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory,
                security,
            },
        )
        .await;
        assert!(matches!(result, Err(TurnServiceBuildError::EventStoreUnsupported)));
        assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
        assert!(!workspace.path().join("self/causal_tree/decisions.db").exists());
    }
}

#[tokio::test]
async fn enabled_agent_rejects_disabled_user_event_recording_before_key_or_store() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    config.memory.events.record_user_messages = false;
    let generation = crate::config::new_shared(config.clone()).pin();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let security = Arc::new(SecurityPolicy::default());
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let result = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory")),
            security,
        },
    )
    .await;
    assert!(matches!(result, Err(TurnServiceBuildError::InboundEventsDisabled)));
    assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
    assert!(!workspace.path().join("self/causal_tree/decisions.db").exists());
}

#[tokio::test]
async fn agent_hint_route_drift_is_refused_before_credential_resolution() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.causal_tree.enabled = false;
    config.workspace_dir = workspace.path().to_path_buf();
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        api_key: None,
        credential_ref: None,
    });
    let generation = crate::config::new_shared(config.clone()).pin();
    let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "hint:fast");
    let matching = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config)).await;
    assert!(matching.resolve_agent_model(&proof).is_ok());
    assert!(
        matching
            .validate_agent_execution_model(&proof, "ollama", "hint:fast")
            .is_ok()
    );
    assert!(
        matching
            .validate_agent_execution_model(&proof, "openai", "gpt-4o-mini")
            .is_ok()
    );
    for (provider, model) in [("ollama", "gpt-4o-mini"), ("openai", "hint:fast"), ("openai", "gpt-4o")] {
        assert!(matches!(
            matching.validate_agent_execution_model(&proof, provider, model),
            Err(TurnModelError::SelectionDrift)
        ));
    }

    let mut foreign = config.clone();
    foreign.model_routes.first_mut().expect("configured route").model = "gpt-4o".to_string();
    let stale = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign)).await;
    assert!(matches!(
        stale.resolve_agent_model(&proof),
        Err(TurnModelError::RouteDrift)
    ));
    assert!(matches!(
        stale.validate_agent_execution_model(&proof, "openai", "gpt-4o-mini"),
        Err(TurnModelError::RouteDrift)
    ));
    assert!(!stale.model_can_call_tools(&proof));

    let route = foreign.model_routes.first_mut().expect("configured route");
    route.model = "gpt-4o-mini".to_string();
    route.credential_ref = Some("cred-other".to_string());
    let wrong_owner = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign)).await;
    assert!(matches!(
        wrong_owner.resolve_agent_model(&proof),
        Err(TurnModelError::RouteDrift)
    ));
    assert!(!wrong_owner.model_can_call_tools(&proof));
}

#[tokio::test]
async fn verified_native_tools_requires_matching_source_selection() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.causal_tree.enabled = false;
    config.workspace_dir = workspace.path().to_path_buf();
    config
        .providers
        .models
        .push(crate::config::schema::ProviderModelConfig {
            provider: "ollama".to_string(),
            model: Some("prx-local-model".to_string()),
            operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
            native_tools: Some(true),
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema_dialect: None,
            tool_call_dialect: None,
            context_window: None,
        });
    let generation = crate::config::new_shared(config.clone()).pin();
    let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "prx-local-model");
    let service = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config)).await;
    assert!(service.resolve_agent_model(&proof).is_ok());
    assert!(service.model_can_call_tools(&proof));

    let mut foreign = config;
    foreign
        .providers
        .models
        .first_mut()
        .expect("configured model")
        .native_tools = Some(false);
    let stale = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign)).await;
    assert!(!stale.model_can_call_tools(&proof));
}

#[tokio::test]
async fn disabled_service_keeps_workspace_key_absent_and_builtin_only() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.causal_tree.enabled = false;
    config.workspace_dir = workspace.path().to_path_buf();
    assert!(!config.causal_tree.enabled);
    let generation = crate::config::new_shared(config.clone()).pin();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy::default()))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let events: Arc<dyn CausalEventSink> = Arc::new(SilentEvents);
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let mut stale_sources = sources(&generation, &config);
    stale_sources.config_generation_id = "stale_generation".to_string();
    let stale = build_turn_decision_service(
        TurnSurfaceKind::Interactive,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry: Arc::clone(&registry),
            execution: Arc::clone(&execution),
            catalog_sources: stale_sources,
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::clone(&events),
            memory: Arc::new(NoneMemory::new()),
            security: Arc::new(SecurityPolicy::default()),
        },
    )
    .await;
    assert!(matches!(stale, Err(TurnServiceBuildError::SourceGeneration)));
    assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
    let bundle = TurnServiceBuild {
        parent_grant: None,
        generation: Arc::clone(&generation),
        registry: Arc::clone(&registry),
        execution: Arc::clone(&execution),
        catalog_sources: sources(&generation, &config),
        surface_policy: SurfacePolicy::default(),
        provider_catalog: &provider_catalog,
        transports: &transports,
        events: Arc::clone(&events),
        memory: Arc::new(NoneMemory::new()),
        security: Arc::new(SecurityPolicy::default()),
    };
    let first = build_turn_decision_service(TurnSurfaceKind::AgentLoop, bundle)
        .await
        .expect("disabled service");
    assert!(!first.durable_key);
    assert!(!first.recordable);
    let status = first.status();
    assert!(!status.backend.enabled);
    assert_eq!(status.catalog_generation, *first.catalog.generation());
    assert_eq!(status.store_mode, StoreMode::ReadOnly);
    assert!(!status.durable_key);
    assert!(!status.recordable);
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "prx-local-model");
    let resolved = first.resolve_agent_model(&proof).expect("pinned PMM model");
    assert_eq!(resolved.model(), "prx-local-model");
    assert!(
        !first.model_can_call_tools(&proof),
        "unverified tools grant no G7 authority"
    );
    let mut other_config = config.clone();
    other_config.default_model = Some("prx_other_model".to_string());
    let other_generation = crate::config::new_shared(other_config).pin();
    let stale_proof = crate::agent::entry::test_cli_agent_turn_proof(&other_generation, "ollama", "prx-local-model");
    assert!(matches!(
        first.resolve_agent_model(&stale_proof),
        Err(TurnModelError::Origin(_))
    ));
    assert_eq!(first.store.mode(), StoreMode::ReadOnly);
    assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
    let second = build_turn_decision_service(
        TurnSurfaceKind::Interactive,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events,
            memory: Arc::new(NoneMemory::new()),
            security: Arc::new(SecurityPolicy::default()),
        },
    )
    .await
    .expect("second disabled service");
    let parts: &[&[u8]] = &[b"prx_turn"];
    assert_ne!(
        first.state_key.hmac(KeyDomain::StateFingerprint, parts),
        second.state_key.hmac(KeyDomain::StateFingerprint, parts)
    );
}

#[tokio::test]
async fn disabled_service_uses_an_independent_read_only_view_when_a_writer_is_shared() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.causal_tree.enabled = false;
    config.workspace_dir = workspace.path().to_path_buf();
    let generation = crate::config::new_shared(config.clone()).pin();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy::default()))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let events: Arc<dyn CausalEventSink> = Arc::new(SilentEvents);
    let store = CausalDecisionStore::open_shared(
        workspace.path(),
        Retention::days(1, 1),
        Arc::clone(&events),
        StoreMode::ReadWrite,
    )
    .expect("writable fixture store");
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let result = build_turn_decision_service(
        TurnSurfaceKind::Interactive,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events,
            memory: Arc::new(NoneMemory::new()),
            security: Arc::new(SecurityPolicy::default()),
        },
    )
    .await;
    let disabled = result.expect("disabled service keeps a separate read-only view");
    assert_eq!(store.mode(), StoreMode::ReadWrite);
    assert_eq!(disabled.store.mode(), StoreMode::ReadOnly);
    assert!(!disabled.recordable);
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
}

#[tokio::test]
async fn enabled_service_with_unavailable_key_does_not_open_a_writable_store() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let key_dir = workspace.path().join("self/causal_tree");
    std::fs::create_dir_all(&key_dir).expect("test key directory");
    std::fs::write(key_dir.join(".decision_key"), b"short").expect("test malformed key");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    let generation = crate::config::new_shared(config.clone()).pin();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy::default()))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let events: Arc<dyn CausalEventSink> = Arc::new(SilentEvents);
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events,
            memory: Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory")),
            security: Arc::new(SecurityPolicy::default()),
        },
    )
    .await
    .expect("builtin-only service");
    assert!(!service.durable_key);
    assert!(!service.recordable);
    assert_eq!(service.store.mode(), StoreMode::ReadOnly);
    let status = service.status();
    assert!(
        status.backend.enabled,
        "configured plane and writable attachment are distinct"
    );
    assert_eq!(status.catalog_generation, *service.catalog.generation());
    assert_eq!(status.store_mode, StoreMode::ReadOnly);
    assert!(!status.durable_key);
    assert!(!status.recordable);
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    assert!(!key_dir.join("decisions.db").exists());
}

#[tokio::test]
async fn validated_usage_and_provider_request_id_reach_the_durable_decision() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("verified local model declaration"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let security = Arc::new(SecurityPolicy::default());
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"));
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .await
    .expect("recordable service");
    let request_ids = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let service_mut = Arc::get_mut(&mut service).expect("unshared service");
    service_mut.factory = DecisionBackendFactory::from_slots(
        vec![Box::new(UsageRecordProbeBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-local-model"),
            },
            request_ids: Arc::clone(&request_ids),
            stall: false,
            entered: Arc::new(tokio::sync::Notify::new()),
        })],
        Arc::clone(&service_mut.state_key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        memory,
        "summarize the neutral record",
        "prx_usage_run",
    )
    .await;
    let context = service
        .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
        .await
        .expect("scripted validated decision");
    let request_id = {
        let ids = request_ids.lock();
        assert_eq!(ids.len(), 1, "one sealed request for the single hop");
        ids.first().cloned().expect("one sealed request id")
    };
    assert_eq!(
        context
            .engine()
            .prediction
            .as_ref()
            .expect("validated prediction")
            .served
            .request_id(),
        request_id,
        "validated request id must equal the sealed manifest"
    );
    let writer = service.store.writer().expect("decision writer");
    assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let db = rusqlite::Connection::open_with_flags(
        workspace.path().join("self/causal_tree/decisions.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read-only decision database");
    let (usage_json, provider_request_id): (Option<String>, Option<String>) = db
        .query_row(
            "SELECT usage_json, provider_request_id FROM decisions WHERE decision_id = ?1",
            [context.decision_id().to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("durable decision row");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&usage_json.expect("reported usage")).expect("usage JSON"),
        serde_json::json!({"input_tokens":17,"output_tokens":3,"cost_micro_usd":42})
    );
    assert_eq!(provider_request_id.as_deref(), Some("prx_provider_request_1"));
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_while_backend_pending_admits_no_decision_or_outcome() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("verified local model declaration"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let security = Arc::new(SecurityPolicy::default());
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"));
    let events = Arc::new(RecordingSink::default());
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::clone(&events) as Arc<dyn CausalEventSink>,
            memory: Arc::clone(&memory),
            security,
        },
    )
    .await
    .expect("recordable service");
    let request_ids = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let service_mut = Arc::get_mut(&mut service).expect("unshared service");
    service_mut.factory = DecisionBackendFactory::from_slots(
        vec![Box::new(UsageRecordProbeBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-local-model"),
            },
            request_ids: Arc::clone(&request_ids),
            stall: true,
            entered: Arc::clone(&entered),
        })],
        Arc::clone(&service_mut.state_key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        memory,
        "summarize the neutral record",
        "prx_cancel_run",
    )
    .await;
    let cancellation = CancellationToken::new();
    let decision = service.decide_agent_initial(&proof, AgentTurnKind::SingleShot, &cancellation);
    let cancel_when_entered = async {
        entered.notified().await;
        cancellation.cancel();
    };
    let ((), result) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(cancel_when_entered, decision)
    })
    .await
    .expect("pending backend reached and cancellation returned");
    assert!(matches!(result, Err(AgentDecisionError::Cancelled)));
    assert_eq!(request_ids.lock().len(), 1);
    let writer = service.store.writer().expect("decision writer");
    assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let counts = service.store.reader().counts().expect("durable counts");
    assert_eq!(counts.decisions, 0);
    assert_eq!(counts.outcomes, 0);
    assert!(
        events
            .events
            .lock()
            .iter()
            .all(|event| event.kind != CausalEventKind::OutcomeRecorded),
        "unadmitted cancellation cannot produce a success outcome"
    );
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn recorded_clarify_and_refuse_each_finalize_once_with_their_own_outcome() {
    use crate::causal_tree::branch::CausalActionKind;

    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    let generation = crate::config::new_shared(config.clone()).pin();
    let changed = Arc::new(AtomicBool::new(false));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(DriftingCatalogTool {
        changed: Arc::clone(&changed),
        dispatches: Arc::clone(&dispatches),
    })]);
    let security = Arc::new(SecurityPolicy::default());
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"));
    let observer = Arc::new(RecordingSink::default());
    let service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: observer.clone(),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .await
    .expect("recordable drift service");
    changed.store(true, Ordering::SeqCst);
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.path().to_string_lossy());
    let mut applied_count = 0;
    for (input, run_id, kind, expected_action, expected_classification) in [
        (
            "  ",
            "prx_clarify_run",
            AgentTurnKind::Repl,
            CausalActionKind::Clarify,
            "clarification",
        ),
        (
            "prx test routing input",
            "prx_refuse_run",
            AgentTurnKind::SingleShot,
            CausalActionKind::Refuse,
            "refusal",
        ),
    ] {
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
            &generation,
            Arc::clone(&memory),
            input,
            run_id,
        )
        .await;
        let mut context = service
            .decide_agent_initial(&proof, kind, &CancellationToken::new())
            .await
            .expect("catalog drift closes through a local terminal");
        assert_eq!(context.terminal_outcome_action(), Some(expected_action));
        let failure = ReceiptClosePorts {
            receipt: None,
            calls: AtomicUsize::new(0),
            cancellation: None,
        };
        let original_directive = context.directive.clone();
        Arc::get_mut(&mut context).expect("unshared Run fixture").directive = TurnExecutionDirective::AnswerDirectly;
        assert!(
            service
                .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(
            failure.calls.load(Ordering::SeqCst),
            0,
            "Run never invokes a closing port"
        );
        Arc::get_mut(&mut context).expect("restore Close fixture").directive = original_directive;
        let original_service_id = context.service_id;
        Arc::get_mut(&mut context).expect("foreign service fixture").service_id = uuid::Uuid::new_v4();
        assert!(
            service
                .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(failure.calls.load(Ordering::SeqCst), 0);
        Arc::get_mut(&mut context).expect("restore service fixture").service_id = original_service_id;
        let original_trusted = context.engine.state.trusted.clone();
        let mutations: [fn(&mut TrustedParts); 8] = [
            |parts| parts.owner_id = Some(Arc::from("prx_foreign_owner")),
            |parts| parts.session_key = Some(Arc::from("prx_foreign_session")),
            |parts| parts.run_id = Some(Arc::from("prx_foreign_run")),
            |parts| parts.source_event_id = Some(Arc::from("prx_foreign_source")),
            |parts| parts.workspace_id = Arc::from("prx_foreign_workspace"),
            |parts| parts.policy_version = Arc::from("prx_foreign_policy"),
            |parts| parts.catalog_generation = CatalogGeneration([0; 32]),
            |parts| parts.channel = Some(Arc::from("prx_foreign_channel")),
        ];
        for mutate in mutations {
            let mut parts = original_trusted.parts().clone();
            mutate(&mut parts);
            Arc::get_mut(&mut context)
                .expect("unshared negative fixture")
                .engine
                .state
                .trusted = TrustedState::from_runtime(parts);
            assert!(
                service
                    .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(
                failure.calls.load(Ordering::SeqCst),
                0,
                "scope mismatch must precede port invocation"
            );
        }
        Arc::get_mut(&mut context)
            .expect("restore exact context")
            .engine
            .state
            .trusted = original_trusted;
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            service
                .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &cancelled)
                .await
                .is_err()
        );
        assert_eq!(failure.calls.load(Ordering::SeqCst), 0);
        assert!(
            service
                .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(failure.calls.load(Ordering::SeqCst), 1, "port errors propagate");

        let terminal_id = format!("{run_id}-terminal");
        let now = chrono::Utc::now();
        let commit = crate::agent::terminal::TurnTerminalDraft {
            decision: Some(
                service
                    .settlement_ref(Arc::clone(&context))
                    .expect("recordable terminal link"),
            ),
            terminal_id: terminal_id.clone(),
            scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                .with_owner_id(proof.owner_id())
                .with_channel("cli")
                .with_session_key(proof.session_key())
                .with_run_id(proof.run_id()),
            status: crate::agent::terminal::TurnTerminalStatus::Completed,
            history: Some(crate::agent::terminal::TurnHistoryProjection {
                assistant_content: "PRX local terminal response".to_owned(),
                history_commit_len: 0,
            }),
            history_scope: None,
            provider_outcome: None,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "local terminal close".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        };
        let ports = FinalizingClosePorts {
            fabric: fabric.clone(),
            commit,
            cost: config.cost.clone(),
            workspace: workspace.path().to_path_buf(),
            calls: AtomicUsize::new(0),
            cancellation: None,
        };
        let cancelled_during_finalize = CancellationToken::new();
        let cancelled_ports = FinalizingClosePorts {
            fabric: ports.fabric.clone(),
            commit: ports.commit.clone(),
            cost: ports.cost.clone(),
            workspace: ports.workspace.clone(),
            calls: AtomicUsize::new(0),
            cancellation: Some(cancelled_during_finalize.clone()),
        };
        assert!(
            service
                .close_agent_directive_turn(
                    &proof,
                    Arc::clone(&context),
                    &cancelled_ports,
                    &cancelled_during_finalize
                )
                .await
                .is_err()
        );
        assert_eq!(cancelled_ports.calls.load(Ordering::SeqCst), 1);
        assert!(
            memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), &format!("turn:{terminal_id}:final"))
                .await
                .expect("committed cancelled-helper marker")
                .is_some()
        );
        assert!(
            memory
                .find_message_event_by_idempotency_key(
                    proof.workspace_id(),
                    &format!("causal:{}:directive-applied", context.decision_id())
                )
                .await
                .expect("no application before valid retry")
                .is_none()
        );
        let first = service
            .close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &CancellationToken::new())
            .await
            .expect("completed local terminal");
        let replay = service
            .close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &CancellationToken::new())
            .await
            .expect("idempotent local terminal replay");
        assert_eq!(
            ports.calls.load(Ordering::SeqCst),
            2,
            "idempotent finalizer receives each retry"
        );
        assert_eq!(first.terminal_event.id, replay.terminal_event.id);
        let concurrent_cancel_a = CancellationToken::new();
        let concurrent_cancel_b = CancellationToken::new();
        let (concurrent_a, concurrent_b) = tokio::join!(
            service.close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &concurrent_cancel_a),
            service.close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &concurrent_cancel_b),
        );
        assert_eq!(
            concurrent_a.expect("concurrent retry").terminal_event.id,
            first.terminal_event.id
        );
        assert_eq!(
            concurrent_b.expect("concurrent retry").terminal_event.id,
            first.terminal_event.id
        );
        let mut forged = first.clone();
        forged.terminal_event.event_id = "prx_unpersisted_event".to_owned();
        for receipt in [forged, {
            let mut failed = first.clone();
            let mut marker: serde_json::Value =
                serde_json::from_str(failed.terminal_event.raw_payload_json.as_deref().expect("marker")).expect("JSON");
            *marker.get_mut("status").expect("terminal marker status") = serde_json::json!("failed");
            failed.terminal_event.raw_payload_json = Some(marker.to_string());
            failed
        }] {
            let returned = ReceiptClosePorts {
                receipt: Some(receipt),
                calls: AtomicUsize::new(0),
                cancellation: None,
            };
            assert!(
                service
                    .close_agent_directive_turn(&proof, Arc::clone(&context), &returned, &CancellationToken::new())
                    .await
                    .is_err()
            );
        }
        let cancelled = CancellationToken::new();
        let returned = ReceiptClosePorts {
            receipt: Some(first.clone()),
            calls: AtomicUsize::new(0),
            cancellation: Some(cancelled.clone()),
        };
        assert!(
            service
                .close_agent_directive_turn(&proof, Arc::clone(&context), &returned, &cancelled)
                .await
                .is_err()
        );
        assert_eq!(returned.calls.load(Ordering::SeqCst), 1);
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        let applied = memory
            .find_message_event_by_idempotency_key(
                proof.workspace_id(),
                &format!("causal:{}:directive-applied", context.decision_id()),
            )
            .await
            .expect("read applied directive")
            .expect("successful close must publish an applied directive");
        assert_eq!(applied.event_type, "causal.directive.applied");
        applied_count += 1;
        assert_eq!(
            observer
                .events
                .lock()
                .iter()
                .filter(|event| event.kind == CausalEventKind::DirectiveApplied)
                .count(),
            applied_count,
            "only durable insertion winners notify the observer"
        );
        assert_eq!(applied.owner_id.as_deref(), Some(proof.owner_id()));
        assert_eq!(applied.session_key.as_deref(), Some(proof.session_key()));
        assert_eq!(applied.run_id.as_deref(), Some(proof.run_id()));
        assert_eq!(
            applied.correlation_id.as_deref(),
            Some(context.decision_id().to_string().as_str())
        );

        assert_eq!(
            applied.causation_event_id.as_deref(),
            Some(first.terminal_event.event_id.as_str())
        );

        assert_eq!(
            first.assistant_event.as_ref().map(|event| event.id),
            replay.assistant_event.as_ref().map(|event| event.id)
        );
        let marker: serde_json::Value = serde_json::from_str(
            first
                .terminal_event
                .raw_payload_json
                .as_deref()
                .expect("terminal marker"),
        )
        .expect("terminal marker JSON");
        assert_eq!(
            marker.get("status").and_then(serde_json::Value::as_str),
            Some("completed")
        );
        assert_eq!(
            marker.get("decision_id").and_then(serde_json::Value::as_str),
            Some(context.decision_id().to_string().as_str())
        );
        let writer = service.store.writer().expect("decision writer");
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let scope = DecisionScope::Owner {
            workspace_id: service.store.workspace_id().expect("store workspace"),
            owner_id: Arc::from(proof.owner_id()),
        };
        let row = service
            .store
            .reader()
            .get(&scope, &context.decision_id())
            .expect("read local terminal decision")
            .expect("local terminal decision row");
        assert_eq!(row.outcomes.len(), 1);
        assert_eq!(
            row.outcomes.first().expect("one terminal outcome").classification,
            expected_classification
        );
        assert_eq!(
            row.outcomes.first().expect("one terminal outcome").settlement_id,
            terminal_id
        );
    }
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
}
