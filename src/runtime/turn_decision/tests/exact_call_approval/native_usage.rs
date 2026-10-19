#![cfg(test)]
//! Real NativeTool discovery and exact pending approval; config reload never dispatches.
use super::*;

struct CountedNativeReload {
    inner: crate::tools::config_reload::ConfigReloadTool,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for CountedNativeReload {
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

#[tokio::test]
async fn native_discovery_approval_retains_reported_usage_in_normal_terminal() {
    let _keyring = crate::acl::approval_grant::WitnessKeyring::global_for_tests();
    let workspace = tempfile::tempdir().unwrap();
    let tool_name = "config_reload";
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.causal_tree.enabled = true;
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
    config.save().await.expect("write owned fixture configuration");
    let shared = crate::config::new_shared(config.clone());
    let generation = shared.pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(crate::tools::file_read::FileReadTool::new(Arc::clone(&security), false)),
        Box::new(CountedNativeReload {
            inner: crate::tools::config_reload::ConfigReloadTool::with_security(shared, Arc::clone(&security)),
            calls: Arc::clone(&dispatches),
        }),
        Box::new(crate::tools::skills::SkillsListTool::new(
            workspace.path().to_owned(),
            config.clone(),
        )),
        Box::new(CapabilitySearchTool),
    ]);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::Interactive,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry: Arc::clone(&registry),
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::from(["file_read".to_owned()]),
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
    .unwrap();
    use futures_util::FutureExt as _;
    let owned_cleanup_confirmed = AtomicBool::new(true);
    let result = std::panic::AssertUnwindSafe(async {
        // Deterministic local decision fixture, not hosted-backend admission
        // or a claim about builtin multilingual semantic understanding.
        let predictions = Arc::new(AtomicUsize::new(0));
        let owning = Arc::get_mut(&mut service).unwrap();
        owning.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(FilesystemApprovalBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-local-model"),
                },
                family: "filesystem",
                calls: Arc::clone(&predictions),
            })],
            Arc::clone(&owning.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        assert!(service.recordable && service.durable_key);
        let (proof, envelope) = recorded_approval_source(
            &generation,
            Arc::clone(&memory),
            "Inspect project context",
            ChatMode::Edit,
            "prx_approval_original",
            1,
        )
        .await;
        assert!(service.model_can_call_tools(proof.as_ref()));
        let cancel = CancellationToken::new();
        let context = service
            .decide_agent_initial(proof.as_ref(), AgentTurnKind::Repl, &cancel)
            .await
            .unwrap();
        assert!(
            predictions.load(Ordering::SeqCst) > 0,
            "the actual controlled local backend must supply predictions"
        );
        assert!(matches!(context.directive(), TurnExecutionDirective::Execute { .. }));
        let selected = service.agent_execution_specs(proof.as_ref(), &context).unwrap();
        assert_eq!(
            selected.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
            ["file_read"]
        );
        let entry = service.catalog.entry_for_public_name(tool_name).unwrap();
        assert_eq!(entry.family, CapabilityTarget::NativeTool);
        assert!(matches!(entry.exposure, Exposure::Searchable));
        let port = service.capability_search_port(proof.as_ref(), &context).unwrap();
        let search = with_search_authority(
            Some(port),
            CapabilitySearchTool.execute(serde_json::json!({"query":tool_name,"limit":1})),
        )
        .await
        .unwrap();
        assert!(search.success);
        let offered: serde_json::Value = serde_json::from_str(&search.output).unwrap();
        assert_eq!(offered.get("hits").unwrap().as_array().unwrap().len(), 1);
        let proposal = service
            .admit_discovered_tool(proof.as_ref(), &context, tool_name)
            .unwrap()
            .unwrap();
        assert!(matches!(
            proposal.admission,
            crate::causal_tree::gates::GateOutcome::ApprovalDeferred
                | crate::causal_tree::gates::GateOutcome::ApprovalOnly { .. }
        ));
        assert!(
            service
                .agent_appended_execution_specs(proof.as_ref(), &context)
                .unwrap()
                .specs
                .is_empty()
        );

        let arguments = serde_json::json!({});
        Box::pin(reported_usage::exercise(reported_usage::Fixture {
            service: &service,
            context: &context,
            proof: proof.as_ref(),
            envelope: &envelope,
            config: &config,
            memory: &memory,
            registry: &registry,
            tool_name,
            arguments: &arguments,
            cleanup_confirmed: &owned_cleanup_confirmed,
        }))
        .await;
        assert!(
            service
                .inspect_pending_decision_approval(proof.owner_id(), proof.session_key(), context.decision_id())
                .await
                .is_ok()
        );
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            0,
            "pending reload must not reach configuration mutation"
        );
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    })
    .catch_unwind()
    .await;
    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    let cleanup = tokio::time::timeout_at(cleanup_deadline.into(), service.drain_writes(cleanup_deadline)).await;
    let confirmed = matches!(cleanup, Ok(true)) && owned_cleanup_confirmed.load(Ordering::SeqCst);
    if !confirmed || result.is_err() {
        let retained = workspace.keep();
        use std::io::Write as _;
        if let Err(error) = writeln!(
            std::io::stdout().lock(),
            "PRX_NATIVE_APPROVAL_FIXTURE_RETAINED {}",
            retained.display()
        ) {
            tracing::error!(%error, "Failed to write retained native approval fixture path");
        }
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(confirmed, "approval fixture final writer cleanup must be confirmed");
}
