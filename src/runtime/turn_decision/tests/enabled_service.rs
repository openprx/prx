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
    assert!(crate::runtime::turn_model::validate_execution(&proof, "ollama", "hint:fast").is_ok());
    assert!(crate::runtime::turn_model::validate_execution(&proof, "openai", "gpt-4o-mini").is_ok());
    for (provider, model) in [("ollama", "gpt-4o-mini"), ("openai", "hint:fast"), ("openai", "gpt-4o")] {
        assert!(
            crate::runtime::turn_model::validate_execution(&proof, provider, model).is_err(),
            "test: mismatched execution model must be refused by the ingress binding"
        );
    }

    let mut foreign = config.clone();
    foreign.model_routes.first_mut().expect("configured route").model = "gpt-4o".to_string();
    let stale = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign)).await;
    assert!(matches!(
        stale.resolve_agent_model(&proof),
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
    config.decision_engine.enabled = true;
    config.causal_tree.backend.kind =
        crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::Builtin);
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
        .open_issued_turn_context(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
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
    config.decision_engine.enabled = true;
    config.causal_tree.backend.kind =
        crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::Builtin);
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
    let decision = service.open_issued_turn_context(&proof, AgentTurnKind::SingleShot, &cancellation);
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
