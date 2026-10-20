#![cfg(test)]
use super::*;

pub(super) struct DriftingCatalogTool {
    pub(super) changed: Arc<AtomicBool>,
    pub(super) dispatches: Arc<AtomicUsize>,
}

struct PerfCatalogTool(ToolSpec);

impl PerfCatalogTool {
    fn new(name: String) -> Self {
        Self(ToolSpec {
            name,
            description: "unrelated PRX performance fixture".to_string(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
        })
    }
}

#[async_trait]
impl Tool for PerfCatalogTool {
    fn name(&self) -> &str {
        &self.0.name
    }

    fn description(&self) -> &str {
        &self.0.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.0.parameters.clone()
    }

    fn immutable_catalog_specs(&self) -> Option<&[ToolSpec]> {
        Some(std::slice::from_ref(&self.0))
    }

    fn catalog_availability_is_configured(&self) -> bool {
        true
    }

    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        anyhow::bail!("performance fixture must not execute a tool")
    }
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::print_stderr)] // Diagnostic numbers belong in the captured test log.
async fn service_builtin_allocation_diagnostic_holds_candidate_count_fixed() {
    let mut measurements = Vec::new();
    // Run both sizes once before comparing them so process-global schema
    // and redaction initialization is outside the measured pair.
    for catalog_size in [64_usize, 0, 64, 0] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.causal_tree.enabled = false;
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config.clone()).pin();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(
            (0..catalog_size)
                .map(|index| Box::new(PerfCatalogTool::new(format!("prx_perf_{index:04}"))) as Box<dyn Tool>)
                .collect(),
        );
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
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
                events: Arc::new(SilentEvents),
                memory: Arc::new(NoneMemory::new()),
                security,
            },
        )
        .await
        .expect("synthetic disabled service");
        let proof = crate::agent::entry::test_cli_agent_turn_proof_with_input(
            &generation,
            "ollama",
            "prx-local-model",
            "summarize the neutral source",
        );
        let cancellation = CancellationToken::new();
        let specs_allocations: u64 = service
            .registry
            .iter()
            .map(|tool| crate::decision::builtin::allocation_counter::count(|| tool.catalog_specs()).1)
            .sum();
        let (_, descriptor_allocations) =
            crate::decision::builtin::allocation_counter::count(|| service.execution.current_descriptors());
        let (parity, parity_allocations) =
            crate::decision::builtin::allocation_counter::count(|| service.check_live_parity());
        parity.expect("fixture catalog parity");
        let model = service.resolve_agent_model(&proof).expect("fixture model");
        let catalog = service.catalog.view(&CatalogScope::default());
        let (_, cost_allocations) = crate::decision::builtin::allocation_counter::count(|| {
            super::super::super::turn_cost::build_cost_model(&catalog, &model, &config.cost, &BTreeMap::new())
        });
        let (result, allocations) = crate::decision::builtin::allocation_counter::count_future(
            service.decide_agent_initial(&proof, AgentTurnKind::SingleShot, &cancellation),
        )
        .await;
        let context = result.expect("builtin service decision");
        let candidates = match &context.engine().candidates {
            crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } => candidates.len(),
            crate::causal_tree::candidate_generator::CandidateSet::NoLegalPath { terminals, .. } => terminals.len(),
        };
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        measurements.push((
            catalog_size,
            candidates,
            specs_allocations,
            descriptor_allocations,
            parity_allocations,
            cost_allocations,
            allocations,
        ));
    }
    let large = measurements.get(2).expect("warmed large catalog result");
    let first = measurements.get(3).expect("warmed small catalog result");
    assert_eq!(first.1, large.1, "candidate count must stay fixed");
    assert!(
        measurements
            .iter()
            .all(|(_, _, _, _, _, _, allocations)| *allocations > 0)
    );
    eprintln!("service allocation diagnostic (caller thread): {measurements:?}");
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::print_stderr)] // Diagnostic numbers belong in the captured test log.
async fn enabled_service_allocation_diagnostic_holds_candidate_count_fixed() {
    let mut measurements = Vec::new();
    // Alternate sizes and compare the warmed pair: global regex and
    // projection initialization must not masquerade as catalog cost.
    for catalog_size in [64_usize, 0, 64, 0] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = true;
        config.causal_tree.policy.extra_latency_budget_ms = 5_000;
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .expect("verified local model fixture"),
        );
        let generation = crate::config::new_shared(config.clone()).pin();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        // Q106: N counts extra inventory beyond one fixed searchable seed (N + 2 tools total).
        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(PerfCatalogTool::new("file_read".to_string())),
            Box::new(PerfCatalogTool::new("prx_perf_0000".to_string())),
        ];
        tools.extend(
            (1..=catalog_size)
                .map(|index| Box::new(PerfCatalogTool::new(format!("prx_perf_{index:04}"))) as Box<dyn Tool>),
        );
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(tools);
        let surface_policy = SurfacePolicy {
            allowed_tools: None,
            direct_names: BTreeSet::from(["file_read".to_string()]),
            ..SurfacePolicy::default()
        };
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"));
        let service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy,
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .await
        .expect("enabled synthetic service");
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof(&generation, memory).await;
        let (result, allocations) = crate::decision::builtin::allocation_counter::count_future(
            service.decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new()),
        )
        .await;
        let context = result.expect("builtin service decision");
        let seed = service
            .catalog
            .view(&CatalogScope::default())
            .entries()
            .iter()
            .find(|entry| entry.public_name == "prx_perf_0000")
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .expect("fixed searchable discovery seed");
        assert!(
            context.audit_hops().any(|hop| matches!(
                &hop.candidates,
                crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. }
                    if candidates.iter().any(|candidate| {
                        candidate.action() == crate::causal_tree::branch::CausalActionKind::Discover
                            && candidate.target() == crate::causal_tree::branch::CapabilityTarget::NativeTool
                            && candidate.required_capabilities() == std::slice::from_ref(&seed)
                    })
            )),
            "each measured context must contain its genuine fixed seed Discover reference"
        );
        let expected = service
            .catalog
            .view(&CatalogScope::default())
            .entries()
            .iter()
            .find(|entry| entry.public_name == "file_read")
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .expect("fixed direct read capability");
        assert!(context.audit_hops().any(|hop| matches!(
            &hop.candidates,
            crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. }
                if candidates.iter().any(|candidate| {
                    candidate.action() == crate::causal_tree::branch::CausalActionKind::UseTool
                        && candidate.required_capabilities() == [expected.clone()]
                })
        )));
        let candidates = match &context.engine().candidates {
            crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } => candidates.len(),
            crate::causal_tree::candidate_generator::CandidateSet::NoLegalPath { terminals, .. } => terminals.len(),
        };
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        let candidate_shape: Vec<_> = context
            .audit_hops()
            .map(|hop| {
                let admitted = match &hop.candidates {
                    crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } => candidates,
                    crate::causal_tree::candidate_generator::CandidateSet::NoLegalPath { terminals, .. } => terminals,
                };
                admitted
                    .iter()
                    .map(|candidate| {
                        (
                            candidate.candidate_id().clone(),
                            candidate.action(),
                            candidate.target(),
                            candidate
                                .required_capabilities()
                                .iter()
                                .map(|reference| (reference.id.clone(), reference.fingerprint))
                                .collect::<Vec<_>>(),
                            candidate.side_effect(),
                            candidate.approval().clone(),
                            candidate.estimated_cost(),
                            candidate.estimated_latency_ms(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let hop_contract: Vec<_> = context
            .audit_hops()
            .map(|hop| {
                (
                    hop.request_compiled,
                    hop.prediction
                        .as_ref()
                        .map(|prediction| format!("{:?}", prediction.served_by)),
                    hop.fallback_cause,
                    hop.scored_count,
                    hop.scores.len(),
                    hop.selection.clone(),
                    format!("{:?}", hop.pre_commit.outcomes),
                )
            })
            .collect();
        let directive = std::mem::discriminant(context.directive());
        measurements.push((
            catalog_size,
            candidates,
            allocations,
            candidate_shape,
            hop_contract,
            directive,
        ));
    }
    let large = measurements.get(2).expect("warmed large catalog measurement");
    let first = measurements.get(3).expect("warmed small catalog measurement");
    assert_eq!(first.1, large.1);
    assert_eq!(first.3, large.3, "each hop must admit the same candidate bodies");
    assert_eq!(first.4, large.4, "each hop must take the same prediction and gate path");
    assert_eq!(first.5, large.5, "the final directive must match");
    eprintln!("enabled service allocation diagnostic (caller thread): {measurements:?}");
}

#[async_trait]
impl Tool for DriftingCatalogTool {
    fn name(&self) -> &str {
        "prx_drift_tool"
    }

    fn description(&self) -> &str {
        if self.changed.load(Ordering::SeqCst) {
            "changed PRX fixture description"
        } else {
            "initial PRX fixture description"
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }

    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("catalog drift must not dispatch a tool")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CatalogDriftMoment {
    BeforeTurn,
    AfterResponse,
    WhileBackendPending,
}

async fn run_catalog_drift_probe(
    moment: CatalogDriftMoment,
    cancel_while_pending: bool,
) -> (Result<Arc<TurnDecisionContext>, AgentDecisionError>, usize, usize) {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.policy.extra_latency_budget_ms = if moment == CatalogDriftMoment::WhileBackendPending {
        500
    } else {
        5_000
    };
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("verified test model declaration"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let memory: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("real fixture SQLite"));
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let changed = Arc::new(AtomicBool::new(false));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(DriftingCatalogTool {
        changed: Arc::clone(&changed),
        dispatches: Arc::clone(&dispatches),
    })]);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
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
    .expect("initial catalog parity");
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let backend_entered = Arc::new(tokio::sync::Notify::new());
    if moment != CatalogDriftMoment::BeforeTurn {
        let service_mut = Arc::get_mut(&mut service).expect("unshared fixture service");
        service_mut.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(RetrieveProbeBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-local-model"),
                },
                requests: Arc::clone(&requests),
                stall: moment == CatalogDriftMoment::WhileBackendPending,
                entered: Arc::clone(&backend_entered),
                drift_after_response: (moment == CatalogDriftMoment::AfterResponse).then(|| Arc::clone(&changed)),
            })],
            Arc::clone(&service_mut.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
    }

    let work = Box::pin(async {
        changed.store(moment == CatalogDriftMoment::BeforeTurn, Ordering::SeqCst);
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
            &generation,
            Arc::clone(&memory),
            "prx_drift_tool",
            "prx_run_drift",
            ("ollama", "prx-local-model"),
        )
        .await;
        let cancellation = CancellationToken::new();
        let decision = service.decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation);
        let context = if moment == CatalogDriftMoment::WhileBackendPending {
            let change_during_wait = async {
                backend_entered.notified().await;
                changed.store(true, Ordering::SeqCst);
                if cancel_while_pending {
                    cancellation.cancel();
                }
            };
            let ((), result) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(change_during_wait, decision)
            })
            .await
            .expect("pending backend must reach bounded fallback");
            result
        } else {
            decision.await
        };
        let request_count = requests.lock().len();
        (context, request_count, dispatches.load(Ordering::SeqCst))
    });
    ledger_probe::finish_service_fixture(&service, work).await
}

#[tokio::test]
async fn catalog_drift_closes_with_fresh_unlinked_terminal_and_no_dispatch() {
    for (moment, expected_requests) in [
        (CatalogDriftMoment::BeforeTurn, 0),
        (CatalogDriftMoment::AfterResponse, 1),
        (CatalogDriftMoment::WhileBackendPending, 1),
    ] {
        let (context, requests, dispatches) = run_catalog_drift_probe(moment, false).await;
        let context = context.expect("catalog drift closes locally");
        assert_eq!(requests, expected_requests);
        assert_eq!(dispatches, 0);
        assert_eq!(context.hop_ids().len(), 1, "stale work hop must be discarded");
        assert_eq!(context.audit_hops().count(), 1);
        assert_eq!(context.engine().state.lineage.prior_decision_id(), None);
        assert!(
            context.engine().prediction.is_none(),
            "recovery must not call a backend"
        );
        assert!(context.execution_refs().is_empty());
        assert!(matches!(
            context.directive(),
            TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. }
        ));
        let rows = DecisionRecordBuilder::build_enabled_chain(&context, &DecisionKey::ephemeral(), true, false)
            .expect("single local terminal audit row");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows.first().expect("one local terminal row").lineage.prior_decision_id,
            None
        );
    }
}

#[tokio::test]
async fn cancellation_during_backend_and_catalog_drift_wins_before_recovery() {
    let (result, requests, dispatches) = run_catalog_drift_probe(CatalogDriftMoment::WhileBackendPending, true).await;
    assert!(matches!(result, Err(AgentDecisionError::Cancelled)));
    assert_eq!(requests, 1);
    assert_eq!(dispatches, 0);
}

#[tokio::test]
async fn catalog_drift_after_selected_read_discards_retrieved_work_chain() {
    let (reads, transports, requests, _, context) =
        run_retrieve_probe_case(true, ProbeCancelMode::CatalogDriftRead).await;
    let context = context.expect("selected read drift closes locally");
    assert_eq!(reads, 1);
    assert_eq!(requests.len(), 1);
    assert_eq!(transports, 0);
    assert_eq!(context.hop_ids().len(), 1);
    assert_eq!(context.engine().state.lineage.prior_decision_id(), None);
    assert!(context.engine().prediction.is_none());
    assert!(context.execution_refs().is_empty());
    assert!(matches!(context.directive(), TurnExecutionDirective::Refuse { .. }));
}
