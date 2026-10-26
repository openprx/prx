#![cfg(test)]
//! Native shell and HTTP calls use the genuine Chat collection/review/resume path.
use super::*;

#[tokio::test]
async fn supervised_shell_collects_exact_arguments_and_executes_once() {
    native_collection(false).await;
}

#[tokio::test]
async fn supervised_http_collects_exact_arguments_without_bypassing_ssrf() {
    native_collection(true).await;
}

async fn native_collection(http: bool) {
    let _keyring = crate::acl::approval_grant::WitnessKeyring::global_for_tests();
    let workspace = tempfile::tempdir().unwrap();
    let tool_name = if http { "http_request" } else { "shell" };
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.causal_tree.enabled = true;
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    config.autonomy.level = crate::security::AutonomyLevel::Supervised;
    config.cost.prices.insert(
        "prx-local-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .unwrap(),
    );
    config.save().await.expect("owned fixture config");
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let tool: Box<dyn Tool> = if http {
        Box::new(crate::tools::http_request::HttpRequestTool::new(
            Arc::clone(&security),
            vec![],
            1024,
            5,
        ))
    } else {
        Box::new(crate::tools::shell::ShellTool::new(
            Arc::new(crate::runtime::NativeRuntime::new()),
            workspace.path().to_owned(),
        ))
    };
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(CountedFileMutation {
        inner: tool,
        calls: Arc::clone(&dispatches),
    })]);
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
    let execution = Arc::new(
        ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        )
        .with_idempotency_memory(Arc::clone(&memory)),
    );
    let catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let service = build_turn_decision_service(
        TurnSurfaceKind::Interactive,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry: Arc::clone(&registry),
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::new(),
                excluded_names: config.tool_tiering.always_exclude.iter().cloned().collect(),
                ..SurfacePolicy::default()
            },
            provider_catalog: &catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .await
    .unwrap();

    let cancel = CancellationToken::new();
    let arguments = if http {
        serde_json::json!({"url":"http://127.0.0.1:1/blocked", "method":"POST", "body":"reviewed", "headers":{"X-PRX-Test":"reviewed"}})
    } else {
        serde_json::json!({"command":"printf reviewed > prx-approved.txt"})
    };
    let (proof, envelope) = recorded_approval_source(
        &generation,
        Arc::clone(&memory),
        &format!("Call {tool_name} with the reviewed arguments"),
        ChatMode::Edit,
        "prx_native_original",
        1,
    )
    .await;
    let context = service
        .decide_agent_initial(proof.as_ref(), AgentTurnKind::Repl, &cancel)
        .await
        .unwrap();
    assert!(context.approval_collection_proof().is_some());
    assert!(context.execution_refs().is_empty());
    let cleanup = AtomicBool::new(true);
    Box::pin(super::usage::exercise(super::usage::Fixture {
        service: &service,
        context: &context,
        proof: proof.as_ref(),
        envelope: &envelope,
        config: &config,
        memory: &memory,
        registry: &registry,
        tool_name,
        arguments: &arguments,
        cleanup_confirmed: &cleanup,
    }))
    .await;
    assert!(cleanup.load(Ordering::SeqCst));
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert!(!workspace.path().join("prx-approved.txt").exists());
    let (owner, session, id) = (proof.owner_id(), proof.session_key(), context.decision_id());
    let pending = service
        .load_pending_decision_approval(owner, session, id)
        .await
        .unwrap();
    assert_eq!(
        pending.op_ids(),
        &[if http {
            "http_request:request:http://127.0.0.1:1".to_owned()
        } else {
            "printf reviewed > prx-approved.txt".to_owned()
        }]
    );
    assert!(
        service
            .prepare_decision_resume(
                owner,
                session,
                id,
                ToolExecutionCommand::new(tool_name, arguments.clone())
            )
            .await
            .is_err()
    );
    assert!(
        service
            .review_pending_decision_approval("prx_other_owner", session, id, true)
            .await
            .is_err()
    );
    service
        .review_pending_decision_approval(owner, session, id, true)
        .await
        .unwrap();
    let changes = if http {
        vec![
            ("url", serde_json::json!("http://127.0.0.1:1/other")),
            ("method", serde_json::json!("DELETE")),
            ("body", serde_json::json!("changed")),
            ("headers", serde_json::json!({"X-PRX-Test":"changed"})),
        ]
    } else {
        vec![("command", serde_json::json!("printf changed > prx-approved.txt"))]
    };
    for (field, value) in changes {
        let mut altered = arguments.clone();
        *altered.get_mut(field).expect("existing argument") = value;
        assert!(
            service
                .prepare_decision_resume(owner, session, id, ToolExecutionCommand::new(tool_name, altered))
                .await
                .is_err()
        );
    }
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    let input = serde_json::json!({"decision_id":id.to_string()}).to_string();
    let (resume, envelope) = recorded_approval_source(
        &generation,
        Arc::clone(&memory),
        &input,
        ChatMode::Edit,
        "prx_native_resume",
        2,
    )
    .await;
    let resumed = service
        .decide_agent_resume(resume.as_ref(), AgentTurnKind::Repl, &cancel)
        .await
        .unwrap();
    let actual_context = ToolExecutionContext::new(envelope, resume.policy_chat_type());
    let result = service
        .execute_agent_resume(resume.as_ref(), &resumed, &actual_context, &cancel)
        .await
        .unwrap();
    assert_eq!(dispatches.load(Ordering::SeqCst), 1, "one approved dispatch");
    if http {
        assert!(
            result.status != ToolExecutionStatus::Succeeded,
            "approval cannot bypass the real HTTP tool's private-host policy"
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Blocked local/private")
        );
    } else {
        assert!(result.status == ToolExecutionStatus::Succeeded, "{:?}", result.error);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("prx-approved.txt")).unwrap(),
            "reviewed"
        );
    }
    assert!(
        service
            .execute_agent_resume(resume.as_ref(), &resumed, &actual_context, &cancel)
            .await
            .is_err()
    );
    let (replay, _) = recorded_approval_source(
        &generation,
        Arc::clone(&memory),
        &input,
        ChatMode::Edit,
        "prx_native_replay",
        3,
    )
    .await;
    assert!(
        service
            .decide_agent_resume(replay.as_ref(), AgentTurnKind::Repl, &cancel)
            .await
            .is_err()
    );
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert!(service.drain_writes(Instant::now() + Duration::from_secs(5)).await);
}
