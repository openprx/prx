#![cfg(test)]
use super::*;

#[test]
fn record_builder_distinguishes_local_terminal_from_failed_compiled_request() {
    use crate::causal_tree::branch::CausalActionKind;
    use crate::causal_tree::candidate_generator::CandidateSet;
    use crate::causal_tree::decision_select::Selection;
    use crate::causal_tree::gates::{GateOutcome, GateReport};
    use crate::causal_tree::state::fixtures;

    let key = fixtures::key();
    let state = fixtures::state(&key, "neutral request");
    let candidate = fixtures::candidate(&key, CausalActionKind::Refuse, CapabilityTarget::None, None).candidate;
    let candidate_id = candidate.candidate_id().clone();
    let gates = GateReport {
        outcomes: BTreeMap::from([(candidate_id.clone(), GateOutcome::Passed)]),
        gate_set_hash: [0; 32],
    };
    let decision_id = DecisionId::new_runtime();
    let engine = EngineDecision {
        decision_id,
        deadline: Instant::now() + Duration::from_secs(1),
        elapsed_ms: 7,
        state,
        candidates: CandidateSet::Legal {
            candidates: vec![candidate],
            exact_mentions: BTreeSet::new(),
            truncated: Vec::new(),
            discoverable_families: Default::default(),
            approval_scopes: BTreeMap::new(),
            report: gates.clone(),
        },
        selection: Selection::Abstain {
            action: CausalActionKind::Refuse,
            reason_code: "hop_stopped",
        },
        exploration_propensities: BTreeMap::new(),
        prediction: None,
        request_compiled: false,
        audit_trace: None,
        fallback_cause: Some(crate::causal_tree::decision_engine::FallbackCause::HopStopped),
        scores: BTreeMap::new(),
        scored_count: 0,
        rehearsal: BTreeMap::new(),
        post_rehearsal: gates.clone(),
        pre_commit: gates,
        call_fingerprints: BTreeMap::new(),
        outgoing_child: None,
        outgoing_parent_allowed: None,
    };
    let mut context = TurnDecisionContext {
        model_catalog: None,
        resume: None,
        accounting: None,
        owner_filesystem: None,
        source_actor: (Arc::from("cli"), Arc::from("local-user")),
        service_id: uuid::Uuid::nil(),
        record_admitted: AtomicBool::new(false),
        recursive_allowance: Default::default(),
        main_parameters: Default::default(),
        execution_trace: std::sync::OnceLock::new(),
        #[cfg(feature = "llm-router")]
        automix_deadline: std::sync::OnceLock::new(),
        served_auxiliary: std::sync::OnceLock::new(),
        execution_closed: AtomicBool::new(false),
        approval_collection_started: AtomicBool::new(false),
        collected_pending: std::sync::OnceLock::new(),
        discovery_offers: std::sync::OnceLock::new(),
        discovery_admissions: std::sync::OnceLock::new(),
        engine,
        prior_hops: Vec::new(),
        directive: TurnExecutionDirective::Refuse {
            policy_code: "hop_stopped".to_owned(),
        },
        main_compaction: crate::config::AgentCompactionConfig::default(),
        hop_ids: vec![decision_id],
        execution_refs: BTreeSet::new(),
        selected_retrieved: Vec::new(),
        selected_custom_memory_context: None,
        #[cfg(feature = "llm-router")]
        router_transition: None,
    };
    use crate::causal_tree::outcome::{DecisionRecordBuilder, RecordBuildError};
    assert!(matches!(
        DecisionRecordBuilder::build_enabled_chain(&context, &key, false, false),
        Err(RecordBuildError::Binding)
    ));
    let local = DecisionRecordBuilder::build_enabled_chain(&context, &key, true, false).expect("local terminal record");
    assert_eq!(local.len(), 1);
    let local_row = local.first().expect("one local terminal");
    assert_eq!(local_row.backend_kind, "none");
    assert_eq!(local_row.selected_candidate_id.as_deref(), Some(candidate_id.as_str()));
    assert_eq!(local_row.protocol_version, 0);
    assert_eq!(local_row.error_class, None);
    assert_eq!(local_row.abstention_reason.as_deref(), Some("hop_stopped"));
    context.engine.request_compiled = true;
    context.engine.fallback_cause = Some(crate::causal_tree::decision_engine::FallbackCause::DeadlineExceeded);
    let timed_out = DecisionRecordBuilder::build_enabled_chain(&context, &key, true, false)
        .expect("compiled request without validated answer");
    let timed_out_row = timed_out.first().expect("one compiled timeout");
    assert_eq!(timed_out_row.backend_kind, "none");
    assert_eq!(
        timed_out_row.protocol_version,
        u32::from(crate::decision::PROTOCOL_VERSION)
    );
    assert_eq!(timed_out_row.error_class, None);
    assert_eq!(timed_out_row.abstention_reason.as_deref(), Some("deadline_exceeded"));
    context.hop_ids.clear();
    assert!(matches!(
        DecisionRecordBuilder::build_enabled_chain(&context, &key, true, false),
        Err(RecordBuildError::Chain)
    ));
}

#[tokio::test]
async fn agent_decision_cancellation_stops_before_backend_or_selected_read() {
    for mode in [ProbeCancelMode::Before, ProbeCancelMode::Backend, ProbeCancelMode::Read] {
        let (reads, transports, requests, emitted, result) = run_retrieve_probe_case(true, mode).await;
        assert!(matches!(result, Err(AgentDecisionError::Cancelled)));
        assert_eq!(transports, 0);
        assert!(emitted.iter().all(|event| {
            !matches!(
                event.kind,
                CausalEventKind::CandidateGated | CausalEventKind::CandidateScored
            ) || mode == ProbeCancelMode::Read
        }));
        match mode {
            ProbeCancelMode::Never | ProbeCancelMode::Before => {
                assert_eq!(reads, 0);
                assert!(requests.is_empty(), "expected an empty value");
            }
            ProbeCancelMode::Backend => {
                assert_eq!(reads, 0);
                assert_eq!(requests.len(), 1);
            }
            ProbeCancelMode::Read => {
                assert_eq!(reads, 1);
                assert_eq!(requests.len(), 1, "cancelled read cannot cause a dependent decision");
            }
            ProbeCancelMode::CatalogDriftRead => {
                panic!("the cancellation test does not use catalog drift")
            }
        }
    }
}

#[tokio::test]
async fn agent_decision_phases_follow_source_lineage_without_claiming_commit() {
    let (_, _, requests, emitted, context) = run_retrieve_probe_case(true, ProbeCancelMode::Never).await;
    let context = context.expect("bounded fixture decision");
    let phases: Vec<_> = emitted
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                CausalEventKind::SnapshotCreated
                    | CausalEventKind::CandidatesGenerated
                    | CausalEventKind::CandidateGated
                    | CausalEventKind::CandidateScored
            )
        })
        .collect();
    assert_eq!(context.hop_ids().len(), requests.len() + 1);
    for decision_id in context.hop_ids() {
        let id = decision_id.to_string();
        let hop: Vec<_> = phases
            .iter()
            .filter(|event| event.lineage.decision_id.as_deref() == Some(id.as_str()))
            .collect();
        assert_eq!(
            hop.first().map(|event| event.kind),
            Some(CausalEventKind::SnapshotCreated)
        );
        assert_eq!(
            hop.get(1).map(|event| event.kind),
            Some(CausalEventKind::CandidatesGenerated)
        );
        assert_eq!(
            hop.get(2).map(|event| event.kind),
            Some(CausalEventKind::CandidateGated)
        );
        assert!(hop.iter().all(|event| {
            event.lineage.run_id.as_deref() == Some("prx_run_retrieve")
                && event.fields.item_count.is_none_or(|count| count <= 32)
        }));
    }
    let terminal = context.hop_ids().last().expect("terminal hop").to_string();
    assert!(phases.iter().any(|event| {
        event.kind == CausalEventKind::CandidateScored && event.fields.item_count.is_some_and(|count| count > 0)
    }));
    assert!(!phases.iter().any(|event| {
        event.kind == CausalEventKind::CandidateScored
            && event.lineage.decision_id.as_deref() == Some(terminal.as_str())
    }));
    assert!(!format!("{phases:?}").contains("find the scoped memory evidence"));
}

#[tokio::test]
async fn selected_retrieve_redecides_only_after_material_evidence_and_honors_hop_cap() {
    let (reads, wire_calls, requests, context) = run_retrieve_probe(true).await;
    assert_eq!(reads, 3, "one selected read per allowed decision hop");
    assert_eq!(
        requests.len(),
        3,
        "terminal hop must not request another backend decision"
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| &request.0)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    assert!(requests.first().expect("first request").1.is_none());
    for (request, prior_id) in requests.iter().skip(1).zip(context.hop_ids()) {
        let prior = prior_id.as_uuid().to_string();
        assert_eq!(request.1.as_deref(), Some(prior.as_str()));
    }
    assert_eq!(context.hop_ids().len(), 4, "three decisions and one terminal gate");
    let recorded: Vec<_> = context.audit_hops().collect();
    assert_eq!(recorded.len(), 4, "each completed hop retains its own audit inputs");
    assert_eq!(
        recorded.iter().map(|hop| hop.decision_id).collect::<Vec<_>>(),
        context.hop_ids()
    );
    assert!(recorded.iter().take(3).all(|hop| hop.prediction.is_some()));
    assert!(
        recorded.last().is_some_and(|hop| hop.prediction.is_none()),
        "local terminal has no backend provenance"
    );
    assert_eq!(wire_calls, 0);

    let (reads, wire_calls, requests, context) = run_retrieve_probe(false).await;
    assert_eq!(reads, 2, "a repeated read may occur once before no-progress detection");
    assert_eq!(
        requests.len(),
        2,
        "no material change must stop another backend request"
    );
    assert_eq!(
        context.hop_ids().len(),
        3,
        "a local terminal gate still closes the turn"
    );
    assert_eq!(wire_calls, 0);
}

#[tokio::test]
async fn memory_rehearsal_rejects_wrong_principal_or_query_before_backend() {
    let memory = CountingProbeMemory {
        calls: AtomicUsize::new(0),
        retrieval_calls: AtomicUsize::new(0),
        return_empty: AtomicBool::new(false),
        return_error: AtomicBool::new(false),
        return_never: AtomicBool::new(false),
        return_stale: AtomicBool::new(false),
        unique_results: AtomicBool::new(false),
        drift_on_read: None,
        read_entered: Arc::new(tokio::sync::Notify::new()),
        retrieval_args: parking_lot::Mutex::new(None),
    };
    let scope = MemoryWriteContext::default();
    let key = DecisionKey::ephemeral();
    let probe = ScopedMemoryRehearsal {
        memory: &memory,
        scope: &scope,
        query: "prx bound query",
        principal: "prx_owner",
        key: &key,
    };
    let query_ref = key.hmac(KeyDomain::StateFingerprint, &[b"rehearsal-query", b"prx bound query"]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    assert!(probe.count_hits("foreign_owner", query_ref, 5, deadline).await.is_err());
    assert!(probe.count_hits("prx_owner", [0; 32], 5, deadline).await.is_err());
    assert!(probe.count_hits("prx_owner", query_ref, 0, deadline).await.is_err());
    assert_eq!(memory.calls.load(Ordering::SeqCst), 0);
    assert_eq!(probe.count_hits("prx_owner", query_ref, 5, deadline).await.unwrap(), 1);
    assert_eq!(memory.calls.load(Ordering::SeqCst), 1);
}

struct RehearsalReadTool {
    name: &'static str,
    configured_only: bool,
}

struct AliasRootTool;

#[async_trait]
impl Tool for AliasRootTool {
    fn name(&self) -> &str {
        "prx_root"
    }

    fn description(&self) -> &str {
        "PRX alias root fixture"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}})
    }

    fn supports_name(&self, name: &str) -> bool {
        matches!(name, "prx_root" | "prx_alias" | "prx_other_alias")
    }

    fn specs(&self) -> Vec<crate::tools::traits::ToolSpec> {
        ["prx_root", "prx_alias", "prx_other_alias"]
            .into_iter()
            .map(|name| crate::tools::traits::ToolSpec {
                name: name.to_string(),
                description: self.description().to_string(),
                parameters: self.parameters_schema(),
            })
            .collect()
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

#[test]
fn agent_model_allowlist_keeps_selected_root_aliases_and_excludes_other_roots() {
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(AliasRootTool),
        Box::new(RehearsalReadTool {
            name: "prx_unrelated",
            configured_only: false,
        }),
    ]);
    let catalog = CapabilityCatalog::build(registry.as_ref(), &CatalogSources::default(), &SurfacePolicy::default())
        .expect("catalog fixture");
    let security = Arc::new(SecurityPolicy::default());
    let execution = ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    );
    catalog
        .check_parity(&execution)
        .expect("publishable root and alias catalog");
    let view = catalog.view(&CatalogScope::default());
    let mut config = crate::config::Config::default();
    config
        .tool_tiering
        .model_allowlists
        .insert("prx-local-model".into(), vec!["prx_alias".into()]);
    let exclusions = effective_agent_exclusions(
        &config,
        registry.as_ref(),
        &view,
        "prx-local-model",
        "prx-policy",
        &crate::causal_tree::state::fixtures::key(),
    );
    assert!(
        !exclusions
            .model_admission
            .as_ref()
            .unwrap()
            .excludes(view.model_admission_entry(catalog.entry_for_public_name("prx_alias").unwrap()))
    );
    assert!(
        !exclusions
            .model_admission
            .as_ref()
            .unwrap()
            .excludes(view.model_admission_entry(catalog.entry_for_public_name("prx_other_alias").unwrap()))
    );
    assert!(
        exclusions
            .model_admission
            .as_ref()
            .unwrap()
            .excludes(view.model_admission_entry(catalog.entry_for_public_name("prx_unrelated").unwrap()))
    );
}

#[async_trait]
impl Tool for RehearsalReadTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "PRX catalog rehearsal fixture"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}})
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        if self.configured_only {
            crate::capability::CapabilityAvailability::configured("fixture config only")
        } else {
            crate::capability::CapabilityAvailability::ready("fixture backend registered")
        }
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

#[tokio::test]
async fn pinned_rehearsal_counts_only_visible_exact_refs_and_skill_family() {
    let registry: Vec<Box<dyn Tool>> = vec![
        Box::new(RehearsalReadTool {
            name: "prx_rehearsal_read",
            configured_only: false,
        }),
        Box::new(RehearsalReadTool {
            name: "prx_rehearsal_configured",
            configured_only: true,
        }),
    ];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
        .expect("catalog fixture");
    let view = catalog.view(&CatalogScope::default());
    let entry = view
        .entries()
        .iter()
        .find(|entry| entry.public_name == "prx_rehearsal_read")
        .expect("ready fixture entry");
    let reference = CapabilityRef {
        id: entry.id.clone(),
        fingerprint: entry.fingerprint,
        generation: entry.generation,
    };
    let probe = PinnedCatalogRehearsal { catalog: &view };
    assert_eq!(
        probe
            .availability(view.generation(), std::slice::from_ref(&reference))
            .await
            .unwrap()
            .present,
        1
    );
    assert_eq!(
        probe
            .metadata(view.generation(), std::slice::from_ref(&reference))
            .await
            .unwrap()
            .present,
        0
    );
    let mut stale = reference.clone();
    stale.fingerprint.0[0] ^= 1;
    assert_eq!(
        probe.availability(view.generation(), &[stale]).await.unwrap().present,
        0
    );
    let mut stale_generation = view.generation();
    stale_generation.0[0] ^= 1;
    assert!(probe.availability(stale_generation, &[]).await.is_err());
    let configured = view
        .entries()
        .iter()
        .find(|entry| entry.public_name == "prx_rehearsal_configured")
        .expect("configured fixture entry");
    let configured_ref = CapabilityRef {
        id: configured.id.clone(),
        fingerprint: configured.fingerprint,
        generation: configured.generation,
    };
    assert_eq!(
        probe
            .availability(view.generation(), &[configured_ref])
            .await
            .unwrap()
            .present,
        0
    );
    let hidden_view = catalog.view(&CatalogScope {
        families: std::iter::once(CapabilityTarget::Memory).collect(),
        channel: false,
    });
    let hidden_probe = PinnedCatalogRehearsal { catalog: &hidden_view };
    assert_eq!(
        hidden_probe
            .availability(hidden_view.generation(), &[reference])
            .await
            .unwrap()
            .present,
        0
    );
}

#[test]
fn decision_token_ratio_reservation_uses_effective_window_and_rejects_tiny_windows() {
    let mut config = crate::config::AgentCompactionConfig::default();
    config.max_context_tokens = 32_768;
    config.reserve_tokens = 4_096;
    let reservation = DecisionTokenReservation::from_effective_compaction(
        &config,
        crate::causal_tree::config::CausalPolicy::default().extra_token_ratio_limit,
    )
    .unwrap();
    assert_eq!(reservation.decision_tokens, 1_792);
    assert_eq!(reservation.main_input_limit, 26_880);
    assert_eq!(reservation.decision_tokens + reservation.main_input_limit, 28_672);
    let reserved = reservation
        .reserved_compaction(
            &config,
            crate::causal_tree::config::CausalPolicy::default().extra_token_ratio_limit,
        )
        .unwrap();
    assert_eq!(reserved.reserve_tokens, 5_888);
    assert_eq!(
        reserved.max_context_tokens - reserved.reserve_tokens,
        reservation.main_input_limit
    );
    assert_eq!(config.reserve_tokens, 4_096, "the effective route config is immutable");
    let mut changed_route = config.clone();
    changed_route.max_context_tokens = 16_384;
    assert!(
        matches!(
            reservation.reserved_compaction(
                &changed_route,
                crate::causal_tree::config::CausalPolicy::default().extra_token_ratio_limit,
            ),
            Err(DecisionReserveUnavailable)
        ),
        "a reservation is bound to its effective input window"
    );

    config.max_context_tokens = 100_000;
    config.reserve_tokens = 1;
    let capped = DecisionTokenReservation::from_effective_compaction(
        &config,
        crate::causal_tree::config::CausalPolicy::default().extra_token_ratio_limit,
    )
    .unwrap();
    assert_eq!(capped.decision_tokens, 4_096);
    assert_eq!(capped.main_input_limit, 95_903);

    config.max_context_tokens = 16;
    config.reserve_tokens = 0;
    let smallest = DecisionTokenReservation::from_effective_compaction(
        &config,
        crate::causal_tree::config::CausalPolicy::default().extra_token_ratio_limit,
    )
    .unwrap();
    assert_eq!(smallest.decision_tokens, 1);
    assert_eq!(smallest.main_input_limit, 15);

    config.max_context_tokens = 32_768;
    config.reserve_tokens = 4_096;
    let ratio_limited = DecisionTokenReservation::from_effective_compaction(&config, 0.01).unwrap();
    assert_eq!(ratio_limited.decision_tokens, 286);
    assert_eq!(ratio_limited.main_input_limit, 28_386);
    assert_eq!(
        DecisionTokenReservation::from_effective_compaction(&config, 0.0),
        Err(DecisionReserveUnavailable),
        "a zero ratio cannot admit an unaccounted decision request"
    );

    for (window, response_reserve) in [(0, 0), (4, 5), (15, 0), (4_096, 4_096)] {
        config.max_context_tokens = window;
        config.reserve_tokens = response_reserve;
        assert_eq!(
            DecisionTokenReservation::from_effective_compaction(
                &config,
                crate::causal_tree::config::CausalPolicy::default().extra_token_ratio_limit,
            ),
            Err(DecisionReserveUnavailable)
        );
    }
}

#[tokio::test]
async fn agent_budget_uses_pinned_route_and_read_only_spend_snapshot() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.causal_tree.enabled = false;
    config.workspace_dir = workspace.path().to_path_buf();
    config.agent.compaction.max_context_tokens = 32_768;
    config.agent.compaction.max_context_tokens_explicit = true;
    config.agent.compaction.reserve_tokens = 4_096;
    config.cost.daily_limit_usd = 100.0;
    config.cost.monthly_limit_usd = 100.0;
    config.autonomy.max_cost_per_day_cents = 1_000;
    let tracker =
        crate::cost::tracker::CostTracker::new(config.cost.clone(), workspace.path()).expect("test cost ledger");
    tracker
        .record_usage(crate::cost::types::TokenUsage::new(
            "test/model",
            1_000_000,
            0,
            9.5,
            0.0,
        ))
        .expect("prior spend");
    let generation = crate::config::new_shared(config.clone()).pin();
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let service = disabled_agent_service(&generation, &provider_catalog).await;
    let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "prx-local-model");
    let snapshot = service.agent_budget_snapshot(&proof).expect("source-owned budget");
    assert_eq!(snapshot.budgets.tokens, 1_792);
    assert_eq!(snapshot.budgets.micro_usd, 500_000);
    assert_eq!(
        snapshot.budgets.latency_ms,
        config.causal_tree.policy.extra_latency_budget_ms
    );
    assert_eq!(snapshot.main_compaction.max_context_tokens, 32_768);
    assert_eq!(snapshot.main_compaction.reserve_tokens, 5_888);
    assert_eq!(config.agent.compaction.reserve_tokens, 4_096);
    assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());

    let mut other_config = config;
    other_config.agent.compaction.max_context_tokens = 15;
    let other_generation = crate::config::new_shared(other_config.clone()).pin();
    let other_service =
        disabled_agent_service(&other_generation, &ProviderCatalogView::from_config(&other_config)).await;
    let other_proof = crate::agent::entry::test_cli_agent_turn_proof(&other_generation, "ollama", "prx-local-model");
    assert!(matches!(
        other_service.agent_budget_snapshot(&other_proof),
        Err(AgentDecisionBudgetError::Reserve(_))
    ));
    assert!(matches!(
        service.agent_budget_snapshot(&other_proof),
        Err(AgentDecisionBudgetError::Model(TurnModelError::Origin(_)))
    ));
}

#[tokio::test]
async fn disabled_agent_initial_decision_runs_generator_gates_and_builtin() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = false;
    let generation = crate::config::new_shared(config.clone()).pin();
    let service = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config)).await;
    let proof = crate::agent::entry::test_cli_agent_turn_proof_with_input(
        &generation,
        "ollama",
        "prx-local-model",
        "Explain `2 + 2`.",
    );
    let result = service
        .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
        .await
        .expect("source-bound builtin decision");
    assert!(matches!(result.directive(), TurnExecutionDirective::Refuse { .. }));
    assert!(matches!(
        &result.engine().candidates,
        crate::causal_tree::candidate_generator::CandidateSet::Legal { .. }
    ));
    assert_eq!(
        result
            .engine()
            .prediction
            .as_ref()
            .map(|prediction| prediction.served_by),
        Some(crate::decision::factory::ServedBy::Builtin)
    );
    assert!(!result.engine().pre_commit.outcomes.is_empty());
    assert_eq!(
        result.main_compaction().max_context_tokens,
        config.agent.compaction.max_context_tokens
    );
    assert_eq!(result.hop_ids(), &[result.decision_id()]);
    assert_eq!(
        result.catalog_generation(),
        result.engine().state.trusted.parts().catalog_generation
    );
    assert_eq!(
        result.inherited_constraint_codes(),
        result.engine().state.trusted.parts().constraints.as_slice()
    );
    assert!(result.execution_refs().is_empty());
    assert!(
        result.retrieved().expect("live selected evidence").is_empty(),
        "expected an empty value"
    );
    assert!(matches!(result.disposition(), Ok(TurnDisposition::Close { .. })));
    // Exercise fresh admission with this actual selected engine decision,
    // not a fabricated selection or an already-consumed context getter.
    let directive_context = DirectiveContext {
        current_generation: result.catalog_generation(),
        approval_expires_at: chrono::Utc::now(),
        discovery_limit: 1,
    };
    assert!(matches!(
        directive_from_selection(result.engine(), &directive_context),
        Ok(TurnExecutionDirective::Refuse { .. })
    ));
    let issued_id = result.decision_id();
    let issued_input = result.engine().state.routing_input.clone();
    let issued_trusted = result.engine().state.trusted.clone();
    let mut expired = result;
    Arc::get_mut(&mut expired)
        .expect("unshared fixture context")
        .engine
        .deadline = Instant::now();
    assert!(matches!(
        directive_from_selection(expired.engine(), &directive_context),
        Ok(TurnExecutionDirective::Refuse { .. })
    ));
    // Fixed local closure remains valid after the decision deadline; it
    // grants no provider execution. Actual terminal closure seals access.
    assert!(
        expired.retrieved().expect("already selected evidence").is_empty(),
        "expected an empty value"
    );
    assert!(matches!(expired.disposition(), Ok(TurnDisposition::Close { .. })));
    assert_eq!(expired.decision_id(), issued_id);
    assert_eq!(expired.engine().state.routing_input, issued_input);
    assert_eq!(expired.engine().state.trusted, issued_trusted);
    let _ = expired.terminal_execution_trace();
    assert!(matches!(expired.retrieved(), Err(DirectiveError::NotAdmitted)));
    // Sealing denies evidence access; rendering a fixed close remains safe.
    assert!(matches!(expired.disposition(), Ok(TurnDisposition::Close { .. })));
    assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
}

#[tokio::test]
async fn disabled_or_keyless_fixed_close_has_no_applied_write() {
    for enabled in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = enabled;
        if enabled {
            let directory = workspace.path().join("self/causal_tree");
            std::fs::create_dir_all(&directory).expect("fixture key directory");
            std::fs::write(directory.join(".decision_key"), b"short").expect("unavailable fixture key");
        }
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("ordinary history memory"));
        let generation = crate::config::new_shared(config.clone()).pin();
        let service = agent_service_with_memory(
            &generation,
            &ProviderCatalogView::from_config(&config),
            Arc::clone(&memory),
        )
        .await;
        let proof = if enabled {
            crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
                &generation,
                Arc::clone(&memory),
                "  ",
                "prx_keyless_close_run",
            )
            .await
        } else {
            crate::agent::entry::test_cli_agent_turn_proof_with_input(&generation, "ollama", "prx-local-model", "  ")
        };
        let context = service
            .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
            .await
            .expect("local fixed decision");
        assert!(matches!(context.disposition(), Ok(TurnDisposition::Close { .. })));
        let decision = if enabled {
            assert!(!service.durable_key, "corrupt key must not issue durable authority");
            assert!(!context.record_admitted.load(Ordering::Acquire));
            assert!(
                service.terminal_settlement_ref(Arc::clone(&context)).is_err(),
                "enabled keyless context must not issue a terminal decision"
            );
            None
        } else {
            Some(
                service
                    .terminal_settlement_ref(Arc::clone(&context))
                    .expect("disabled service issues a genuine same-context terminal decision"),
            )
        };
        let now = chrono::Utc::now();
        let ports = FinalizingClosePorts {
            fabric: MemoryFabric::new(Arc::clone(&memory), proof.workspace_id()),
            commit: crate::agent::terminal::TurnTerminalDraft {
                decision,
                terminal_id: "prx_unrecorded_close".to_owned(),
                scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                    .with_owner_id(proof.owner_id())
                    .with_channel("cli")
                    .with_session_key(proof.session_key())
                    .with_run_id(proof.run_id()),
                status: crate::agent::terminal::TurnTerminalStatus::Completed,
                history: None,
                history_scope: None,
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: "local close".to_owned(),
                    started_at: now,
                    finished_at: now,
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
            },
            cost: config.cost.clone(),
            workspace: workspace.path().to_path_buf(),
            calls: AtomicUsize::new(0),
            cancellation: None,
        };
        let close = service
            .close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &CancellationToken::new())
            .await;
        if enabled {
            let error = close.expect_err("enabled keyless close must fail at terminal linkage");
            assert_eq!(error.to_string(), "terminal draft has no service-issued decision");
            assert!(
                memory
                    .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:prx_unrecorded_close:final")
                    .await
                    .expect("keyless terminal marker lookup")
                    .is_none(),
                "keyless close must not persist a terminal marker"
            );
        } else {
            close.expect("ordinary close stays available");
        }
        assert_eq!(ports.calls.load(Ordering::SeqCst), 1);
        assert!(!context.record_admitted.load(Ordering::Acquire));
        assert!(service.durable_events.is_none());
        assert!(
            memory
                .find_message_event_by_idempotency_key(
                    proof.workspace_id(),
                    &format!("causal:{}:directive-applied", context.decision_id())
                )
                .await
                .expect("no applied write")
                .is_none()
        );
        assert!(!workspace.path().join("self/causal_tree/decisions.db").exists());
    }
}

#[tokio::test]
async fn agent_initial_decision_respects_configured_four_kib_wire_limit() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.causal_tree.enabled = false;
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.backend.max_request_bytes = 4096;
    let generation = crate::config::new_shared(config.clone()).pin();
    let service = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config)).await;
    let raw = "PRX route evidence 01. ".repeat(1_000);
    let proof =
        crate::agent::entry::test_cli_agent_turn_proof_with_input(&generation, "ollama", "prx-local-model", &raw);
    assert!(matches!(
        service
            .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
            .await,
        Err(AgentDecisionError::Engine(EngineError::Compiler(
            crate::causal_tree::questions::CompileError::Budget
        )))
    ));
    let mut wide = config;
    wide.causal_tree.backend.max_request_bytes = 65_536;
    let wide_generation = crate::config::new_shared(wide.clone()).pin();
    let wide_service = disabled_agent_service(&wide_generation, &ProviderCatalogView::from_config(&wide)).await;
    let wide_proof =
        crate::agent::entry::test_cli_agent_turn_proof_with_input(&wide_generation, "ollama", "prx-local-model", &raw);
    let wide_result = wide_service
        .decide_agent_initial(&wide_proof, AgentTurnKind::SingleShot, &CancellationToken::new())
        .await
        .expect("the same projected request fits the wider configured cap");
    assert_eq!(
        wide_result
            .engine()
            .prediction
            .as_ref()
            .map(|prediction| prediction.served_by),
        Some(crate::decision::factory::ServedBy::Builtin)
    );
    assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
}
