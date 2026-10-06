#![cfg(test)]
//! Additional owned-ingress profile, separate from the frozen phase matrix.
use super::*;

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit isolated process profile; run one configured N per process"]
async fn owned_model_allowlist_catalog_growth_profile() {
    let extra: usize = std::env::var("PRX_CDP_ALLOWLIST_EXTRA")
        .expect("explicit N")
        .parse()
        .expect("numeric N");
    assert!([0, 1, 16, 64, 256, 512].contains(&extra));
    let workspace = tempfile::tempdir().expect("isolated workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    config
        .tool_tiering
        .model_allowlists
        .insert("prx-local-model".into(), vec!["file_read".into()]);
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("local model fixture"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut tools = vec![
        tool("file_read", &[], &dispatches),
        tool("image_info", &[], &dispatches),
    ];
    for n in 0..extra {
        tools.push(tool(&format!("prx_policy_{n:04}"), &[], &dispatches));
    }
    let registry = Arc::new(tools);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite"));
    let service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                allowed_tools: None,
                direct_names: BTreeSet::from(["file_read".into()]),
                ..SurfacePolicy::default()
            },
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .expect("actual published service");
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        "inspect file_read project evidence",
        "prx_policy_run",
    )
    .await;
    drain_required(&service).await;
    let cancellation = CancellationToken::new();
    let ((context, elapsed_us), allocations) =
        crate::decision::builtin::allocation_counter::count_process_future(async {
            let started = Instant::now();
            let context = service
                .decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation)
                .await
                .expect("actual decision");
            drain_required(&service).await;
            (context, started.elapsed().as_micros())
        })
        .await
        .expect("isolated process allocation window");
    assert!(
        allocations <= 8278,
        "frozen allocation ceiling remains unchanged: {allocations}"
    );
    // Preserve the frozen decide-plus-drain measurement above. This separate
    // post-window check proves matching useful work, not a cheaper refusal.
    assert!(matches!(context.directive(), TurnExecutionDirective::Execute { .. }));
    let selection = service
        .agent_execution_specs(&proof, &context)
        .expect("actual selected execution provenance outside measured window");
    assert_eq!(
        selection
            .specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        ["file_read"]
    );
    let trusted = context.engine.state.trusted.parts();
    assert!(trusted.model_admission.is_some());
    let view = service.catalog_view();
    let allowed = service
        .catalog
        .entry_for_public_name("file_read")
        .expect("allowed root");
    let denied = service
        .catalog
        .entry_for_public_name("image_info")
        .expect("denied root");
    assert!(!trusted.excludes_entry(&view, allowed));
    assert!(trusted.excludes_entry(&view, denied));
    let semantics = semantic_context(&service, &proof, &context);
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    use std::io::Write;
    writeln!(std::io::stdout().lock(), "PRX_CDP_MODEL_ALLOWLIST {}", json!({
        "schema":"owned-model-allowlist-profile-v1", "extra_inventory":extra,
        "rust_allocation_attempts":allocations, "decision_and_drains_us":elapsed_us,
        "semantics":semantics, "scope":"single N; zero-growth and percentile acceptance require independent matrix comparison"
    })).expect("profile evidence output");
}
