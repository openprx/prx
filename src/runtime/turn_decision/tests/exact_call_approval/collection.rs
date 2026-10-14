#![cfg(test)]
//! Actual recorded Chat write-only initiation, owner review and exact fresh resume.
use super::*;
#[path = "collection_usage.rs"]
mod usage;

#[tokio::test]
async fn fresh_chat_file_write_collects_then_reviews_and_resumes_once() {
    Box::pin(exact_file_collection(false)).await;
}
#[tokio::test]
async fn fresh_chat_file_edit_collects_then_reviews_and_resumes_once() {
    Box::pin(exact_file_collection(true)).await;
}

async fn exact_file_collection(edit: bool) {
    let _keyring = crate::acl::approval_grant::WitnessKeyring::global_for_tests();
    let workspace = tempfile::tempdir().unwrap();
    let tool_name = if edit { "file_edit" } else { "file_write" };
    let original = "prefix\noriginal contents\nsuffix\n";
    if edit {
        std::fs::write(workspace.path().join("prx-approved.txt"), original).unwrap();
    }
    std::fs::write(workspace.path().join("prx-unrelated.txt"), "unrelated contents").unwrap();
    let assert_unmodified = || {
        if edit {
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("prx-approved.txt")).unwrap(),
                original
            );
        } else {
            assert!(!workspace.path().join("prx-approved.txt").exists());
        }
    };
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
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(CountedFileMutation {
        inner: if edit {
            Box::new(crate::tools::file_edit::FileEditTool::new(Arc::clone(&security)))
        } else {
            Box::new(FileWriteTool::new(Arc::clone(&security)))
        },
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
    let mut service = build_turn_decision_service(
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
    .unwrap();
    use futures_util::FutureExt as _;
    let owned_cleanup_confirmed = AtomicBool::new(true);
    let result = std::panic::AssertUnwindSafe(Box::pin(async {
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
            &format!("Use {tool_name} to update the project file"),
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
        assert_eq!(predictions.load(Ordering::SeqCst), 0, "collection is code-issued, no prediction action");
        let collection = context.approval_collection_proof().expect("real Chat write-only collection");
        let reference = collection.capability().clone();
        assert!(context.execution_refs().is_empty());
        assert!(service.agent_execution_specs(proof.as_ref(), &context).is_err());
        assert!(service.agent_prompt_skills(proof.as_ref(), &context).unwrap().is_empty());
        assert!(service.capability_search_port(proof.as_ref(), &context).is_err());
        let (initial_plan, _) = recorded_approval_source(
            &generation, Arc::clone(&memory), &format!("Use {tool_name} for the project file"),
            ChatMode::Plan, "prx_collection_initial_plan", 8,
        ).await;
        let denied_plan = service.decide_agent_initial(initial_plan.as_ref(), AgentTurnKind::Repl, &cancel).await.unwrap();
        assert!(denied_plan.approval_collection_proof().is_none());
        assert!(service.begin_approval_collection(initial_plan.as_ref(), &denied_plan).is_err());
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        assert_unmodified();
        let unsupported = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
            &generation, Arc::clone(&memory), &format!("Use {tool_name} for project contents"), "prx_collection_cli_denied",
        ).await;
        assert!(unsupported.approval_review_adapter().is_none(), "generic CLI has no review adapter");
        assert!(service.begin_approval_collection(&unsupported, &context).is_err());
        let writer = service.store.writer().unwrap();
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let arguments = if edit {
            serde_json::json!({"path":"prx-approved.txt","old_string":"original contents","new_string":"approved exact contents","replace_all":false})
        } else {
            serde_json::json!({"path":"prx-approved.txt","content":"approved exact contents"})
        };
        let mut altered = arguments.clone();
        *altered
            .get_mut(if edit { "new_string" } else { "content" })
            .expect("fixture mutation argument exists") = serde_json::json!("altered contents");
        if edit {
            let scope = crate::agent::turn_contracts::ScopeContext {
                policy: &service.security,
                sender: proof.policy_sender(),
                channel: proof.policy_channel(),
                chat_type: proof.policy_chat_type(),
                chat_id: proof.session_key(),
                owner_id: Some(proof.owner_id()),
                topic_id: None,
                task_id: proof.task_id(),
                source_message_event_id: proof.source_event_id(),
                config_generation_id: Some(proof.config_generation_id()),
                config_source_revision: Some(proof.config_source_revision()),
            };
            assert!(
                crate::agent::execution_policy::runtime_approval_grant_for_call(tool_name, &arguments, Some(&scope))
                    .is_none(),
                "legacy callback must not mint a wildcard edit grant even with a real scope and keyring"
            );
        }
        for mode in 0..4 {
            let (bad_proof, bad_envelope) = recorded_approval_source(
                &generation, Arc::clone(&memory), &format!("Use {tool_name} for project contents"),
                ChatMode::Edit, &format!("prx_collection_rejected_{mode}"), 10 + u64::from(mode),
            ).await;
            let bad_context = service.decide_agent_initial(bad_proof.as_ref(), AgentTurnKind::Repl, &cancel).await.unwrap();
            assert!(bad_context.approval_collection_proof().is_some());
            assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
            Box::pin(usage::reject(usage::Fixture {
                service: &service, context: &bad_context, proof: bad_proof.as_ref(), envelope: &bad_envelope,
                config: &config, memory: &memory, registry: &registry, tool_name, arguments: &arguments,
                cleanup_confirmed: &owned_cleanup_confirmed,
            }, mode)).await;
            assert_eq!(dispatches.load(Ordering::SeqCst), 0);
            assert_unmodified();
        }
        Box::pin(usage::exercise(usage::Fixture {
            service: &service, context: &context, proof: proof.as_ref(), envelope: &envelope,
            config: &config, memory: &memory, registry: &registry, tool_name, arguments: &arguments,
            cleanup_confirmed: &owned_cleanup_confirmed,
        })).await;
        assert!(service.begin_approval_collection(proof.as_ref(), &context).is_err(), "lease cannot be reissued");
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        assert_unmodified();
        let owner = proof.owner_id();
        let session = proof.session_key();
        let id = context.decision_id();
        assert!(
            service
                .inspect_pending_decision_approval(owner, session, id)
                .await
                .is_ok()
        );
        assert!(
            service
                .review_pending_decision_approval("prx_foreign_owner", session, id, true)
                .await
                .is_err()
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
        service
            .review_pending_decision_approval(owner, session, id, true)
            .await
            .unwrap();
        assert!(
            service
                .prepare_decision_resume(owner, session, id, ToolExecutionCommand::new(tool_name, altered))
                .await
                .is_err()
        );
        let resume_input = serde_json::json!({"decision_id":id.to_string()}).to_string();

        let (plan, plan_envelope) = recorded_approval_source(
            &generation,
            Arc::clone(&memory),
            &resume_input,
            ChatMode::Plan,
            "prx_approval_plan",
            2,
        )
        .await;
        assert_eq!(plan.owner_id(), owner);
        assert_eq!(plan.session_key(), session);
        assert_eq!(plan.policy_sender(), proof.policy_sender());
        assert!(plan.intercepts_writes());
        let plan_context = service
            .decide_agent_resume(plan.as_ref(), AgentTurnKind::Repl, &cancel)
            .await
            .unwrap();
        assert!(matches!(
            plan_context.disposition().unwrap(),
            TurnDisposition::Close { .. }
        ));
        let resume_authority = plan_context
            .resume
            .as_ref()
            .expect("real approved resume authenticated before G9");
        let evidence = resume_authority.gate_evidence(&service, plan.as_ref()).unwrap();
        assert!(!evidence.approvals.is_empty());
        assert!(
            plan_context
                .engine
                .state
                .trusted
                .parts()
                .constraints
                .iter()
                .any(|code| code.as_str() == "intercepts_writes")
        );
        // Generator may prune a Plan-incompatible write before final reports. Recheck
        // that exact actual approved reference against the unchanged actual state.
        let entries = service
            .catalog
            .resolve(std::slice::from_ref(&reference))
            .unwrap();
        let entry = entries.first().unwrap();
        let model = service.resolve_agent_model(plan.as_ref()).unwrap();
        let model_ref = format!("{}:{}", model.provider_label(), model.model());
        let latency = BTreeMap::new();
        let costs = crate::runtime::turn_cost::RuntimeCostModel::new(&model, &config, &latency);
        let estimate = crate::causal_tree::candidate_generator::CostEstimateSource::estimate_entry(&costs, entry);
        let candidate = crate::causal_tree::branch::CausalCandidate::new(
            crate::causal_tree::branch::CausalActionKind::UseTool,
            entry.family,
            crate::causal_tree::branch::CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference.clone()],
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: estimate.map(|(cost, _)| cost),
                estimated_latency_ms: estimate.map(|(_, latency)| latency),
            },
        );
        let replayed = BTreeSet::new();
        let gates = crate::causal_tree::gates::GateInputs {
            policy: &service.security,
            owner_id: owner,
            sender: plan.policy_sender(),
            policy_channel: plan.policy_channel(),
            chat_type: plan.policy_chat_type(),
            model_ref: &model_ref,
            model_can_chat: true,
            model_can_call_tools: service.model_can_call_tools(plan.as_ref()),
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: evidence.approvals,
            call_fingerprints: &evidence.calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &replayed,
            retry_limit: Some(0),
        };
        assert!(matches!(
            crate::causal_tree::gates::HardGates::check_one(
                crate::causal_tree::gates::GatePass::PreCommit,
                &plan_context.engine.state,
                &service.catalog_view(),
                &candidate,
                &gates
            ),
            crate::causal_tree::gates::GateOutcome::Blocked {
                gate: 9,
                reason_code: "explicit_restriction"
            }
        ));
        assert!(
            service
                .execute_agent_resume(
                    plan.as_ref(),
                    &plan_context,
                    &ToolExecutionContext::new(plan_envelope, plan.policy_chat_type()),
                    &cancel
                )
                .await
                .is_err()
        );
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        assert_unmodified();

        let (resume, resume_envelope) = recorded_approval_source(
            &generation,
            Arc::clone(&memory),
            &resume_input,
            ChatMode::Edit,
            "prx_approval_resume",
            3,
        )
        .await;
        // A stored decision-id string is not authority at ordinary ingress.
        let ordinary = service
            .decide_agent_initial(resume.as_ref(), AgentTurnKind::Repl, &cancel)
            .await
            .unwrap();
        assert!(ordinary.approval_collection_proof().is_none());
        assert!(ordinary.execution_refs().is_empty());
        if let crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } = &ordinary.engine.candidates {
            assert!(candidates.iter().all(|candidate| !candidate.required_capabilities().contains(&reference)));
        }
        let resumed = service
            .decide_agent_resume(resume.as_ref(), AgentTurnKind::Repl, &cancel)
            .await
            .unwrap();
        let exact_candidates = match &resumed.engine.candidates {
            crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } => candidates,
            _ => panic!("authenticated resume must retain a legal candidate set"),
        };
        let exact = exact_candidates.iter().find(|candidate| {
            candidate.required_capabilities() == std::slice::from_ref(&reference)
        }).expect("authenticated resume must retain exactly its capability reference");
        assert!(matches!(
            resumed.engine.pre_commit.outcomes.get(exact.candidate_id()),
            Some(crate::causal_tree::gates::GateOutcome::Passed)
        ));
        assert!(matches!(resumed.disposition().unwrap(), TurnDisposition::Run));
        let selected = service.agent_execution_specs(resume.as_ref(), &resumed).unwrap();
        assert_eq!(
            selected.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
            [tool_name]
        );
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let actual_context = ToolExecutionContext::new(resume_envelope, resume.policy_chat_type());
        let executed = service
            .execute_agent_resume(resume.as_ref(), &resumed, &actual_context, &cancel)
            .await
            .unwrap();
        assert_eq!(executed.status, ToolExecutionStatus::Succeeded, "{:?}", executed.error);
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("prx-approved.txt")).unwrap(),
            if edit {
                "prefix\napproved exact contents\nsuffix\n"
            } else {
                "approved exact contents"
            }
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("prx-unrelated.txt")).unwrap(),
            "unrelated contents"
        );
        assert!(
            service
                .execute_agent_resume(resume.as_ref(), &resumed, &actual_context, &cancel)
                .await
                .is_err()
        );
        let (replay, _) = recorded_approval_source(
            &generation,
            Arc::clone(&memory),
            &resume_input,
            ChatMode::Edit,
            "prx_approval_replay",
            4,
        )
        .await;
        assert!(
            service
                .decide_agent_resume(replay.as_ref(), AgentTurnKind::Repl, &cancel)
                .await
                .is_err()
        );
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        assert!(predictions.load(Ordering::SeqCst) > 0, "fresh resume uses real selected execution");
    }))
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
            "approval collection fixture evidence retained at {}",
            retained.display()
        ) {
            tracing::error!(%error, "Failed to write retained approval collection fixture path");
        }
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(confirmed, "approval fixture final writer cleanup must be confirmed");
}
