#![cfg(test)]
use super::*;

#[tokio::test]
async fn selected_memory_service_rejects_foreign_turn_and_keeps_direct_core_after_read() {
    use crate::causal_tree::branch::{
        ApprovalRequirement, CandidateSpec, CapabilityRef, CapabilityTarget, CausalActionKind, CausalCandidate,
        SideEffectClass,
    };
    use crate::causal_tree::candidate_generator::CandidateSet;
    use crate::causal_tree::decision_select::Selection;
    use crate::causal_tree::gates::{GateOutcome, GateReport};
    use crate::causal_tree::state::{RawRoutingInput, TrustedState, fixtures};

    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("verified retrieval fixture model"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let memory = Arc::new(CountingProbeMemory {
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
    });
    let ledger_memory: Arc<dyn Memory> = Arc::new(ledger_probe::LedgerProbeMemory {
        ledger: Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("real fixture SQLite")),
        probe: Arc::clone(&memory),
    });
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(crate::tools::memory_search::MemorySearchTool::new(
            workspace.path().to_path_buf(),
            Arc::clone(&ledger_memory),
            false,
        )),
        Box::new(crate::tools::file_read::FileReadTool::new(Arc::clone(&security), false)),
    ]);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let transports = CountingTransports(AtomicUsize::new(0));
    let service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::from(["file_read".to_owned()]),
                ..SurfacePolicy::default()
            },
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&ledger_memory),
            security,
        },
    )
    .await
    .expect("SQLite-backed fixture service");
    let work = Box::pin(async {
        let proof_a = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
            &generation,
            Arc::clone(&ledger_memory),
            "same query",
            "prx_run_a",
            ("ollama", "prx-local-model"),
        )
        .await;
        let proof_b = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
            &generation,
            Arc::clone(&ledger_memory),
            "same query",
            "prx_run_b",
            ("ollama", "prx-local-model"),
        )
        .await;
        let model = service.resolve_agent_model(&proof_a).expect("pinned model");
        let view = service.catalog.view(&CatalogScope::default());
        let entry = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "memory_search")
            .expect("read catalog anchor");
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let mut trusted = fixtures::trusted();
        trusted.surface = TurnSurfaceKind::AgentLoop;
        trusted.channel = None;
        trusted.workspace_id = Arc::from(proof_a.workspace_id());
        trusted.owner_id = Some(Arc::from(proof_a.owner_id()));
        trusted.session_key = Some(Arc::from(proof_a.session_key()));
        trusted.run_id = Some(Arc::from(proof_a.run_id()));
        trusted.source_event_id = proof_a.source_event_id().map(Arc::from);
        trusted.catalog_generation = view.generation();
        trusted.policy_version = Arc::clone(&service.policy_version);
        trusted.resolved_model = Arc::from(format!("{}:{}", model.provider_label(), model.model()));
        let effective = effective_agent_exclusions(
            &config,
            &service.registry,
            &view,
            model.model(),
            &service.policy_version,
            &service.state_key,
        );
        trusted.exclusions = effective.names;
        trusted.model_admission = effective.model_admission;
        let mut state = fixtures::state(&service.state_key, "same query");
        state.trusted = TrustedState::from_runtime(trusted);
        state.observed = ObservedState::from_runtime_records(Vec::new(), Vec::new(), 0);
        state.routing_input = RawRoutingInput::new("same query".to_string());
        let candidate = CausalCandidate::new(
            CausalActionKind::Retrieve,
            CapabilityTarget::Memory,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference],
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        );
        let id = candidate.candidate_id().clone();
        let report = GateReport {
            outcomes: BTreeMap::from([(id.clone(), GateOutcome::Passed)]),
            gate_set_hash: [0; 32],
        };
        let mut decision = EngineDecision {
            decision_id: DecisionId::new_runtime(),
            deadline: Instant::now() + Duration::from_secs(5),
            elapsed_ms: 0,
            state,
            candidates: CandidateSet::Legal {
                candidates: vec![candidate],
                exact_mentions: BTreeSet::new(),
                truncated: Vec::new(),
                discoverable_families: Default::default(),
                approval_scopes: BTreeMap::new(),
                report: report.clone(),
            },
            selection: Selection::Commit {
                candidate_id: id.clone(),
                propensity: 1.0,
            },
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: None,
            fallback_cause: None,
            scores: BTreeMap::new(),
            scored_count: 0,
            rehearsal: BTreeMap::new(),
            post_rehearsal: report.clone(),
            pre_commit: report,
            call_fingerprints: BTreeMap::new(),
            outgoing_child: None,
            outgoing_parent_allowed: None,
        };
        assert!(matches!(
            service.observe_selected_agent_memory(&proof_b, &decision).await,
            Err(AgentRetrievalError::Origin(_))
        ));
        assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 0);
        decision.state.lineage = HopLineage::After {
            hop: NonZeroU8::new(config.causal_tree.policy.max_decision_hops).expect("positive fixture limit"),
            prior_decision_id: decision.decision_id.as_uuid(),
        };
        assert!(matches!(
            service.observe_selected_agent_memory(&proof_a, &decision).await,
            Err(AgentRetrievalError::HopLimit)
        ));
        assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 0);
        decision.state.lineage = HopLineage::First;
        let observed = service
            .observe_selected_agent_memory(&proof_a, &decision)
            .await
            .expect("selected read and runtime observation");
        assert!(observed.fresh_read);
        assert_eq!(observed.selected_artifacts.len(), 1);
        let next = observed.state;
        assert_eq!(next.observed.artifacts().len(), 1);
        let [step] = next.observed.completed_steps() else {
            panic!("one runtime-owned read step");
        };
        assert_eq!(step.kind(), ObservedStepKind::Retrieve);
        assert_eq!(step.status(), ObservedStepStatus::Ok);
        assert_eq!(step.candidate(), Some(&id));
        assert_eq!(step.event_id(), &None);
        assert_eq!(next.lineage.hop(), 1);
        assert_eq!(next.lineage.prior_decision_id(), Some(decision.decision_id.as_uuid()));
        assert!(next.derived.candidates().is_empty(), "expected an empty value");
        assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 1);
        {
            let args = memory.retrieval_args.lock();
            let (query, limit, scope, deadline) = args.as_ref().expect("source-scoped read arguments");
            assert_eq!(query, proof_a.routing_input());
            assert_eq!(*limit, 5);
            assert_eq!(scope.workspace_id, proof_a.memory_scope().workspace_id);
            assert_eq!(scope.channel, proof_a.memory_scope().channel);
            assert_eq!(scope.chat_type, proof_a.memory_scope().chat_type);
            assert_eq!(scope.chat_id, proof_a.memory_scope().chat_id);
            assert_eq!(scope.sender_id, proof_a.memory_scope().sender_id);
            assert_eq!(scope.owner_id, proof_a.memory_scope().owner_id);
            assert_eq!(scope.raw_sender, proof_a.memory_scope().raw_sender);
            assert_eq!(scope.require_owner_match, proof_a.memory_scope().require_owner_match);
            assert_eq!(*deadline, decision.deadline);
        }

        let call_fingerprints = BTreeMap::new();
        let replayed_non_idempotent = BTreeSet::new();
        let verified_approval_calls = BTreeMap::new();
        let gates = GateInputs {
            policy: &service.security,
            owner_id: proof_a.owner_id(),
            sender: "local-user",
            policy_channel: "cli",
            chat_type: "private",
            model_ref: next.trusted.parts().resolved_model.as_ref(),
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls,
            call_fingerprints: &call_fingerprints,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &replayed_non_idempotent,
            retry_limit: Some(0),
        };
        let cost_model =
            super::super::super::turn_cost::build_cost_model(&view, &model, &config.cost, &BTreeMap::new());
        let discovered_refs = DiscoveredRefs::empty();
        let rehearsal_ports = RehearsalPorts {
            memory: None,
            catalog: None,
            skill: None,
        };
        let inputs = EngineInputs {
            state: &next,
            catalog: &view,
            config: &config.causal_tree,
            key: &service.state_key,
            decision_id: DecisionId::new_runtime(),
            cost_model: &cost_model,
            discovered_refs: &discovered_refs,
            gates: &gates,
            delegation: None,
            delegation_max_depth: 0,
            prediction: service.factory.prediction_port(None),
            rehearsal_ports: &rehearsal_ports,
            request_limits: RequestLimits::default(),
            model: Some(model.model()),
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            terminal_only: true,
            allow_terminal_answer: true,
            deadline: decision.deadline,
            inject_builtin_failure: false,
            after_prediction_pause: None,
        };
        let terminal = CausalDecisionEngine::decide_terminal_prepared(
            CausalDecisionEngine::prepare_hop(&inputs).expect("fresh terminal preparation"),
        )
        .expect("fresh terminal selection");
        assert!(matches!(
            directive_from_selection(
                &terminal,
                &DirectiveContext {
                    current_generation: view.generation(),
                    approval_expires_at: chrono::Utc::now(),
                    discovery_limit: 1,
                },
            ),
            Ok(TurnExecutionDirective::AnswerDirectly)
        ));
        assert!(terminal.prediction.is_none(), "local terminal makes no backend call");

        let execution_refs = direct_answer_refs(&terminal, &view, &gates, &BTreeSet::new());
        let file_read_ref = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "file_read")
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .expect("direct file-read reference");
        assert!(
            execution_refs.contains(&file_read_ref),
            "a successful Retrieve must keep the Direct tool core for the serving model"
        );

        let answer_context = TurnDecisionContext {
            model_catalog: service.provider_catalog.release_pin(),
            resume: None,
            accounting: None,
            owner_filesystem: None,
            source_actor: (Arc::from("cli"), Arc::from("local-user")),
            service_id: service.service_id,
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
            hop_ids: vec![decision.decision_id, terminal.decision_id],
            prior_hops: Vec::new(),
            engine: terminal,
            directive: TurnExecutionDirective::AnswerDirectly,
            main_compaction: config.agent.compaction.clone(),
            execution_refs,
            selected_retrieved: observed.selected_artifacts.clone(),
            selected_custom_memory_context: None,
            #[cfg(feature = "llm-router")]
            router_transition: None,
        };
        let specs = service
            .agent_execution_specs(&proof_a, &answer_context)
            .expect("retrieval answer Direct specs");
        assert!(
            specs.specs.iter().any(|spec| spec.name == "file_read"),
            "the production request after Retrieve must still publish the Direct file tool"
        );
        assert!(matches!(answer_context.disposition(), Ok(TurnDisposition::Run)));
        assert_eq!(answer_context.retrieved().expect("live selected evidence").len(), 1);
        let close_spy = CloseSpy::default();
        let mut answer_context = Arc::new(answer_context);
        assert!(
            close_directive_turn(Arc::clone(&answer_context), &close_spy)
                .await
                .is_err()
        );
        assert!(close_spy.replies.lock().is_empty(), "Run cannot call the terminal port");
        let answer_context_mut = Arc::get_mut(&mut answer_context).expect("exclusive context after close check");
        answer_context_mut.engine.deadline = Instant::now();
        // Decision work has ended; the admitted turn retains its selected
        // evidence until the real terminal lifecycle seals execution.
        assert!(matches!(answer_context.disposition(), Ok(TurnDisposition::Run)));
        assert_eq!(
            answer_context
                .retrieved()
                .expect("selected evidence before terminal closure")
                .len(),
            1
        );
        let _ = answer_context.terminal_execution_trace();
        assert!(matches!(answer_context.disposition(), Err(DirectiveError::NotAdmitted)));
        assert!(matches!(answer_context.retrieved(), Err(DirectiveError::NotAdmitted)));
        assert!(
            close_directive_turn(Arc::clone(&answer_context), &close_spy)
                .await
                .is_err()
        );
        assert!(
            close_spy.replies.lock().is_empty(),
            "expired Run cannot call the terminal port"
        );
        // Even a corrupted internal reason code cannot escape through Close.
        Arc::get_mut(&mut answer_context)
            .expect("exclusive context after close check")
            .directive = TurnExecutionDirective::Refuse {
            policy_code: "HIDDEN-CAP-9V".to_string(),
        };
        let TurnDisposition::Close { reply } = answer_context.disposition().expect("fixed close reply") else {
            panic!("refusal must close");
        };
        assert_eq!(reply, "I can't complete this request under the current policy.");
        assert!(!reply.contains("HIDDEN-CAP-9V"));
        assert_eq!(
            close_directive_turn(Arc::clone(&answer_context), &close_spy)
                .await
                .expect("fixed refusal reaches terminal port"),
            "fixture terminal receipt"
        );
        assert_eq!(close_spy.replies.lock().as_slice(), &[reply.as_ref()]);
        close_spy.fail.store(true, Ordering::SeqCst);
        assert!(
            close_directive_turn(Arc::clone(&answer_context), &close_spy)
                .await
                .expect_err("terminal failure must propagate")
                .to_string()
                .contains("fixture terminal failure")
        );
        assert_eq!(close_spy.replies.lock().as_slice(), &[reply.as_ref(), reply.as_ref()]);
        close_spy.fail.store(false, Ordering::SeqCst);
        Arc::get_mut(&mut answer_context)
            .expect("exclusive context after close check")
            .directive = TurnExecutionDirective::Clarify {
            reason_code: "HIDDEN-CAP-9V".to_string(),
        };
        close_directive_turn(Arc::clone(&answer_context), &close_spy)
            .await
            .expect("clarification reaches terminal port");
        assert_eq!(close_spy.replies.lock().len(), 3);
        assert_eq!(
            close_spy.replies.lock().get(2).map(String::as_str),
            Some("Could you clarify the result you want?")
        );
        Arc::get_mut(&mut answer_context)
            .expect("exclusive context after close check")
            .directive = TurnExecutionDirective::Discover {
            families: Vec::new(),
            query: "fixture".to_string(),
            limit: 1,
        };
        assert!(
            close_directive_turn(Arc::clone(&answer_context), &close_spy)
                .await
                .is_err()
        );
        assert_eq!(close_spy.replies.lock().len(), 3, "unsupported directive cannot close");

        memory.return_empty.store(true, Ordering::SeqCst);
        let empty_observed = service
            .observe_selected_agent_memory(&proof_a, &decision)
            .await
            .expect("empty selected read still records a completed step");
        assert!(!empty_observed.fresh_read);
        assert!(
            empty_observed.completed_ok,
            "an empty read that completed Ok is a finished retrieval"
        );
        assert!(empty_observed.selected_artifacts.is_empty(), "expected an empty value");
        let mut empty_comparison = empty_observed.state.clone();
        empty_comparison.derived = decision.state.derived.clone();
        let mut reduced = empty_comparison.trusted.parts().clone();
        reduced.budgets.latency_ms = 1;
        empty_comparison.trusted = TrustedState::from_runtime(reduced);
        assert_eq!(
            empty_comparison.evidence_fingerprint(&service.state_key),
            decision.state.evidence_fingerprint(&service.state_key),
            "an empty successful read stays no-progress as latency shrinks"
        );
        let empty_next = empty_observed.state;
        assert!(empty_next.observed.artifacts().is_empty(), "expected an empty value");
        assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 2);
        let empty_inputs = EngineInputs {
            state: &empty_next,
            decision_id: DecisionId::new_runtime(),
            allow_terminal_answer: empty_observed.fresh_read || empty_observed.completed_ok,
            ..inputs.clone()
        };
        let empty_terminal = CausalDecisionEngine::decide_terminal_prepared(
            CausalDecisionEngine::prepare_hop(&empty_inputs).expect("empty terminal preparation"),
        )
        .expect("empty terminal selection");
        assert!(matches!(
            directive_from_selection(
                &empty_terminal,
                &DirectiveContext {
                    current_generation: view.generation(),
                    approval_expires_at: chrono::Utc::now(),
                    discovery_limit: 1,
                },
            ),
            Ok(TurnExecutionDirective::AnswerDirectly)
        ));
        assert!(
            empty_terminal.prediction.is_none(),
            "empty recovery makes no backend call"
        );

        memory.return_empty.store(false, Ordering::SeqCst);
        memory.return_stale.store(true, Ordering::SeqCst);
        let stale_observation = service
            .observe_selected_agent_memory(&proof_a, &decision)
            .await
            .expect("stale selected read remains auditable");
        assert!(!stale_observation.fresh_read);
        assert!(
            stale_observation.selected_artifacts.is_empty(),
            "stale rows cannot enter prompt context"
        );
        assert_eq!(stale_observation.state.observed.artifacts().len(), 1);
        assert!(
            stale_observation
                .state
                .observed
                .artifacts()
                .first()
                .expect("one stale observation")
                .age_ms
                > STALE_ARTIFACT_MS
        );
        assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 3);

        memory.return_stale.store(false, Ordering::SeqCst);
        memory.return_error.store(true, Ordering::SeqCst);
        let prior_fresh = next
            .observed
            .artifacts()
            .first()
            .cloned()
            .expect("fresh prior artifact");
        decision.state.observed = ObservedState::from_runtime_records(Vec::new(), vec![prior_fresh], 0);
        let failed_observation = service
            .observe_selected_agent_memory(&proof_a, &decision)
            .await
            .expect("backend failure closes through an observed step");
        assert!(!failed_observation.fresh_read, "prior evidence cannot authorize Answer");
        assert!(
            !failed_observation.completed_ok,
            "a failed read is not a finished retrieval"
        );
        assert!(
            failed_observation.selected_artifacts.is_empty(),
            "expected an empty value"
        );
        assert_eq!(failed_observation.state.observed.artifacts().len(), 1);
        let [failed_step] = failed_observation.state.observed.completed_steps() else {
            panic!("one failed runtime-owned read step");
        };
        assert_eq!(failed_step.status(), ObservedStepStatus::Failed);
        assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 4);
        let failed_inputs = EngineInputs {
            state: &failed_observation.state,
            decision_id: DecisionId::new_runtime(),
            allow_terminal_answer: failed_observation.fresh_read || failed_observation.completed_ok,
            ..inputs.clone()
        };
        let failed_terminal = CausalDecisionEngine::decide_terminal_prepared(
            CausalDecisionEngine::prepare_hop(&failed_inputs).expect("failed terminal preparation"),
        )
        .expect("failed terminal selection");
        assert!(matches!(
            directive_from_selection(
                &failed_terminal,
                &DirectiveContext {
                    current_generation: view.generation(),
                    approval_expires_at: chrono::Utc::now(),
                    discovery_limit: 1,
                },
            ),
            Ok(TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. })
        ));
        assert!(
            failed_terminal.prediction.is_none(),
            "failed recovery makes no backend call"
        );

        memory.return_never.store(true, Ordering::SeqCst);
        decision.deadline = Instant::now() + Duration::from_secs(2);
        let cancelled = service
            .observe_selected_agent_memory(&proof_a, &decision)
            .await
            .expect("selected read timeout closes through an observed step");
        assert!(!cancelled.fresh_read);
        assert!(!cancelled.completed_ok);
        assert!(cancelled.selected_artifacts.is_empty(), "expected an empty value");
        let [cancelled_step] = cancelled.state.observed.completed_steps() else {
            panic!("one cancelled runtime-owned read step");
        };
        assert_eq!(cancelled_step.status(), ObservedStepStatus::Cancelled);
        assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 5);
        let cancelled_inputs = EngineInputs {
            state: &cancelled.state,
            decision_id: DecisionId::new_runtime(),
            allow_terminal_answer: cancelled.fresh_read || cancelled.completed_ok,
            deadline: decision.deadline,
            ..inputs.clone()
        };
        let cancelled_terminal = CausalDecisionEngine::decide_terminal_prepared(
            CausalDecisionEngine::prepare_hop(&cancelled_inputs).expect("cancelled terminal preparation"),
        )
        .expect("cancelled terminal selection");
        assert!(matches!(
            directive_from_selection(
                &cancelled_terminal,
                &DirectiveContext {
                    current_generation: view.generation(),
                    approval_expires_at: chrono::Utc::now(),
                    discovery_limit: 1,
                },
            ),
            Ok(TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. })
        ));
        assert!(cancelled_terminal.prediction.is_none());
    });
    ledger_probe::finish_service_fixture(&service, work).await
}

#[tokio::test]
async fn builtin_agent_discovery_runs_a_bounded_dependent_hop() {
    struct FriendlierTool(Arc<AtomicUsize>);
    #[async_trait]
    impl Tool for FriendlierTool {
        fn name(&self) -> &str {
            "skills_list"
        }

        fn description(&self) -> &str {
            "Make text sound a little friendlier"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }

        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("discovery must not execute a tool")
        }
    }

    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("verified test model declaration"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let calls = Arc::new(AtomicUsize::new(0));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(FriendlierTool(Arc::clone(&calls)))]);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let memory: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("real fixture SQLite"));
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
            memory: Arc::clone(&memory),
            security,
        },
    )
    .await
    .expect("SQLite-backed fixture service");
    let work =
        Box::pin(async {
            let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&memory),
                "could you make that sound a little friendlier please",
                "prx_run_discover",
                ("ollama", "prx-local-model"),
            )
            .await;
            assert!(
                service.model_can_call_tools(&proof),
                "declared model supports native tools"
            );
            let catalog = service.catalog.view(&CatalogScope::default());
            let discovered_entry = catalog
                .entries()
                .iter()
                .find(|entry| entry.public_name == "skills_list")
                .expect("read-only searchable fixture");
            assert_eq!(
                discovered_entry.effect,
                crate::causal_tree::branch::SideEffectClass::ReadOnly
            );
            assert!(matches!(discovered_entry.exposure, Exposure::Searchable));
            let context = service
                .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
                .await
                .expect("bounded builtin decision");
            let discovery_decision = context
                .audit_hops()
                .find(|decision| {
                    let candidate_id = match &decision.selection {
                        Selection::Commit { candidate_id, .. } | Selection::Recover { candidate_id, .. } => {
                            candidate_id
                        }
                        _ => return false,
                    };
                    let CandidateSet::Legal { candidates, .. } = &decision.candidates else {
                        return false;
                    };
                    candidates.iter().any(|candidate| {
                        candidate.candidate_id() == candidate_id && candidate.action() == CausalActionKind::Discover
                    })
                })
                .expect("bounded path selected discovery");
            assert!(discovery_source_guards_clear(&discovery_decision.state));

            let mut pending_state = discovery_decision.state.clone();
            let mut pending_trusted = pending_state.trusted.parts().clone();
            pending_trusted.approval_pending = true;
            pending_state.trusted = TrustedState::from_runtime(pending_trusted);
            assert!(
                !discovery_source_guards_clear(&pending_state),
                "pending approval must block a terminal discovery answer"
            );

            let mut exact_state = discovery_decision.state.clone();
            let mut derived = exact_state.derived.candidates().to_vec();
            derived
                .first_mut()
                .expect("selected discovery carries derived candidates")
                .exact_mention = true;
            exact_state.derived = DerivedState::new(derived).expect("exact derived fixture state");
            assert!(
                !discovery_source_guards_clear(&exact_state),
                "an exact capability mention must block a terminal discovery answer"
            );
            assert!(
                context.hop_ids().len() >= 2,
                "selected discovery should lead to a dependent hop: {:?}; candidates={:?}; gates={:?}",
                context.directive(),
                context.engine().candidates,
                context.engine().pre_commit.outcomes
            );
            assert!(
                context.hop_ids().len() <= usize::from(config.causal_tree.policy.max_decision_hops) + 1,
                "the configured hop cap bounds local discovery"
            );
            let prior = context
                .hop_ids()
                .iter()
                .rev()
                .nth(1)
                .copied()
                .expect("dependent hop has a predecessor");
            assert_eq!(
                context.engine().state.lineage.prior_decision_id(),
                Some(prior.as_uuid())
            );
            assert!(
                context.engine().state.observed.completed_steps().iter().any(|step| {
                    step.kind() == ObservedStepKind::Discover && step.status() == ObservedStepStatus::Ok
                }),
                "the dependent hop must carry the selected discovery observation"
            );
            assert!(matches!(
                context.disposition(),
                Ok(TurnDisposition::Close { .. } | TurnDisposition::Run)
            ));
            assert_eq!(
                transports.0.load(Ordering::SeqCst),
                0,
                "builtin discovery makes no network transport"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "selected discovery never executes a tool"
            );
        });
    ledger_probe::finish_service_fixture(&service, work).await
}

#[tokio::test]
async fn selected_discovery_is_source_bound_and_never_dispatches_a_tool() {
    use crate::causal_tree::branch::{
        ApprovalRequirement, CandidateSpec, CapabilityRef, CapabilityTarget, CausalActionKind, CausalCandidate,
        SideEffectClass,
    };
    use crate::causal_tree::candidate_generator::CandidateSet;
    use crate::causal_tree::decision_select::Selection;
    use crate::causal_tree::gates::{GateOutcome, GateReport};
    use crate::causal_tree::state::{RawRoutingInput, TrustedState, fixtures};

    struct NoDispatchTool(Arc<AtomicUsize>);
    #[async_trait]
    impl Tool for NoDispatchTool {
        fn name(&self) -> &str {
            "prx_searchable"
        }

        fn description(&self) -> &str {
            "Searchable PRX fixture"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }

        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("selected discovery must not execute a tool")
        }
    }

    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let calls = Arc::new(AtomicUsize::new(0));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(NoDispatchTool(Arc::clone(&calls)))]);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let memory: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("real fixture SQLite"));
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
            memory: Arc::clone(&memory),
            security,
        },
    )
    .await
    .expect("SQLite-backed fixture service");
    let work = Box::pin(async {
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
            &generation,
            Arc::clone(&memory),
            "prx_searchable",
            "prx_run_a",
            ("ollama", "prx-local-model"),
        )
        .await;
        let foreign = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
            &generation,
            Arc::clone(&memory),
            "prx_searchable",
            "prx_run_b",
            ("ollama", "prx-local-model"),
        )
        .await;
        let model = service.resolve_agent_model(&proof).expect("pinned model");
        let view = service.catalog.view(&CatalogScope::default());
        let entry = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "prx_searchable")
            .expect("searchable catalog entry");
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let mut trusted = fixtures::trusted();
        trusted.surface = TurnSurfaceKind::AgentLoop;
        trusted.channel = None;
        trusted.workspace_id = Arc::from(proof.workspace_id());
        trusted.owner_id = Some(Arc::from(proof.owner_id()));
        trusted.session_key = Some(Arc::from(proof.session_key()));
        trusted.run_id = Some(Arc::from(proof.run_id()));
        trusted.source_event_id = proof.source_event_id().map(Arc::from);
        trusted.catalog_generation = view.generation();
        trusted.policy_version = Arc::clone(&service.policy_version);
        trusted.resolved_model = Arc::from(format!("{}:{}", model.provider_label(), model.model()));
        let effective = effective_agent_exclusions(
            &config,
            &service.registry,
            &view,
            model.model(),
            &service.policy_version,
            &service.state_key,
        );
        trusted.exclusions = effective.names;
        trusted.model_admission = effective.model_admission;
        let mut state = fixtures::state(&service.state_key, "prx_searchable");
        state.trusted = TrustedState::from_runtime(trusted);
        state.routing_input = RawRoutingInput::new("prx_searchable".to_string());
        let candidate = CausalCandidate::new(
            CausalActionKind::Discover,
            CapabilityTarget::NativeTool,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference.clone()],
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        );
        let id = candidate.candidate_id().clone();
        let report = GateReport {
            outcomes: BTreeMap::from([(id.clone(), GateOutcome::Passed)]),
            gate_set_hash: [0; 32],
        };
        let mut decision = EngineDecision {
            decision_id: DecisionId::new_runtime(),
            deadline: Instant::now() + Duration::from_secs(5),
            elapsed_ms: 0,
            state,
            candidates: CandidateSet::Legal {
                candidates: vec![candidate],
                exact_mentions: BTreeSet::new(),
                truncated: Vec::new(),
                discoverable_families: Default::default(),
                approval_scopes: BTreeMap::new(),
                report: report.clone(),
            },
            selection: Selection::Commit {
                candidate_id: id,
                propensity: 1.0,
            },
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: None,
            fallback_cause: None,
            scores: BTreeMap::new(),
            scored_count: 0,
            rehearsal: BTreeMap::new(),
            post_rehearsal: report.clone(),
            pre_commit: report,
            call_fingerprints: BTreeMap::new(),
            outgoing_child: None,
            outgoing_parent_allowed: None,
        };
        assert!(matches!(
            service.discover_selected_agent_refs(&foreign, &decision, &view),
            Err(AgentDiscoveryError::Origin(_))
        ));
        assert!(
            service
                .discover_selected_agent_refs(&proof, &decision, &view)
                .expect("selected local discovery")
                .contains(&reference)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "discovery is metadata-only");
        decision.deadline = Instant::now();
        assert!(matches!(
            service.discover_selected_agent_refs(&proof, &decision, &view),
            Err(AgentDiscoveryError::Selected(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
    ledger_probe::finish_service_fixture(&service, work).await
}
