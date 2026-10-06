#![cfg(test)]
//! Additional owned-ingress profile, separate from the frozen phase matrix.
use super::*;

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit isolated process profile; run one configured N per process"]
async fn owned_mcp_inventory_catalog_growth_profile() {
    let extra: usize = std::env::var("PRX_CDP_MCP_EXTRA")
        .expect("explicit N")
        .parse()
        .expect("numeric N");
    assert!([0, 1, 16, 64, 256, 512].contains(&extra));
    let workspace = tempfile::tempdir().expect("isolated workspace");
    let ((service, proof, mcp, transports, dispatches, setup_us), setup_allocations) =
        crate::decision::builtin::allocation_counter::count_process_future(async {
            let setup_started = Instant::now();
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_path_buf();
            config.causal_tree.enabled = true;
            assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
            config
                .mcp
                .servers
                .insert("prx_inventory".into(), crate::config::McpServerConfig::default());
            let server = config.mcp.servers.get("prx_inventory").expect("fixture server");
            assert!(server.command.is_none() && server.url.is_none());
            config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("local model fixture"),
    );
            let generation = crate::config::new_shared(config.clone()).pin();
            let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
            let dispatches = Arc::new(AtomicUsize::new(0));
            let mcp = Arc::new(crate::tools::McpTool::new(
                Arc::clone(&security),
                config.mcp.clone(),
                workspace.path().to_path_buf(),
            ));
            mcp.test_publish_inventory("prx_inventory", extra)
                .expect("actual snapshot publication without transport");
            let mut tools = vec![tool("file_read", &[], &dispatches)];
            tools.push(mcp.test_boxed_tool());
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
            let catalog_sources = CatalogSources::from_runtime_inputs(
                generation.id.0.to_string(),
                &config,
                mcp.capability_snapshot(),
                &hooks,
                0,
            );
            assert_eq!(catalog_sources.mcp.len(), extra);
            let service = build_turn_decision_service(
                TurnSurfaceKind::AgentLoop,
                TurnServiceBuild {
                    parent_grant: None,
                    generation: Arc::clone(&generation),
                    registry,
                    execution,
                    catalog_sources,
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
                "prx_mcp_inventory_run",
            )
            .await;
            drain_required(&service).await;
            (
                service,
                proof,
                mcp,
                transports,
                dispatches,
                setup_started.elapsed().as_micros(),
            )
        })
        .await
        .expect("isolated setup allocation window");
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
    let live_aliases = service
        .catalog_view()
        .iter_entries()
        .filter(|entry| entry.root == "mcp_call" && entry.public_name != "mcp_call")
        .count();
    assert_eq!(live_aliases, extra, "actual published MCP aliases");
    let semantics = semantic_context(&service, &proof, &context);
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(mcp.test_invocation_count(), 0);
    use std::io::Write;
    writeln!(std::io::stdout().lock(), "PRX_CDP_MCP_INVENTORY {}", json!({
        "schema":"owned-mcp-inventory-profile-v1", "extra_inventory":extra,
        "rust_allocation_attempts":allocations, "decision_and_drains_us":elapsed_us,
        "setup_rust_allocation_attempts":setup_allocations, "setup_and_drains_us":setup_us,
        "setup_scope":"config, real MCP publication, registry/service/SQLite/owned ingress and required drains; tempdir creation excluded",
        "provider_transport_calls":transports.0.load(Ordering::SeqCst),
        "file_fixture_dispatches":dispatches.load(Ordering::SeqCst),
        "actual_mcp_invocations":mcp.test_invocation_count(),
        "binary_scope":"instrumented debug-test fixture; production release performance is separate",
        "published_mcp_aliases":live_aliases, "selected_k":1, "extra_latency_budget_ms":500, "allocation_ceiling":8278,
        "semantics":semantics, "scope":"single N; zero-growth and percentile acceptance require independent matrix comparison"
    })).expect("profile evidence output");
}
