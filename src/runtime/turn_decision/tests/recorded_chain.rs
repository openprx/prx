#![cfg(test)]
use super::*;

#[tokio::test]
async fn enabled_agent_service_records_only_a_successful_decision_chain() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.cost.prices.insert(
        "prx_test/model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
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
    .expect("recordable service");
    assert!(service.durable_key && service.recordable);
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof(&generation, Arc::clone(&memory)).await;
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        service
            .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &cancelled)
            .await,
        Err(AgentDecisionError::Cancelled)
    ));
    assert_eq!(service.store.reader().counts().expect("counts").decisions, 0);

    let mut context = service
        .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
        .await
        .expect("successful local decision");
    {
        let mutable = Arc::get_mut(&mut context).expect("exclusive returned context");
        assert!(mutable.record_admitted.swap(false, Ordering::AcqRel));
    }
    assert!(
        service.settlement_ref(Arc::clone(&context)).is_err(),
        "unadmitted context cannot be linked"
    );
    {
        let mutable = Arc::get_mut(&mut context).expect("exclusive returned context");
        mutable.record_admitted.store(true, Ordering::Release);
        mutable.service_id = uuid::Uuid::nil();
    }
    assert!(
        service.settlement_ref(Arc::clone(&context)).is_err(),
        "a foreign service context cannot be linked"
    );
    Arc::get_mut(&mut context)
        .expect("exclusive returned context")
        .service_id = service.service_id;
    let writer = service.store.writer().expect("writable decision store");
    for n in 1_u8..=5 {
        let decision_id = DecisionId::new_runtime();
        let mut row = crate::causal_tree::store::testing::record(
            decision_id,
            proof.workspace_id(),
            Some(proof.owner_id()),
            chrono::Utc::now().timestamp_millis() + i64::from(n),
        );
        row.catalog_generation = context.catalog_generation().to_hex();
        assert_eq!(writer.record_decision(row), crate::causal_tree::store::Enqueue::Queued);
        assert!(
            writer.drain(Instant::now() + Duration::from_secs(5)).await,
            "decision row must commit before its synthetic outcome"
        );
        crate::causal_tree::store::testing::write_single_call_latency(
            workspace.path(),
            decision_id,
            &format!("prx_latency_{n}"),
            "native_tool:prx_project:read",
            u32::from(n) * 10,
        );
    }
    assert_eq!(
        service
            .observed_capability_latency(proof.owner_id(), context.catalog_generation())
            .await
            .get("native_tool:prx_project:read"),
        Some(&30),
        "the service must use the store's stable workspace ID"
    );
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), proof.workspace_id());
    let now = chrono::Utc::now();
    let terminal = crate::agent::terminal::finalize_turn(
        &fabric,
        crate::agent::terminal::TurnTerminalDraft {
            decision: None,
            terminal_id: "unlinked-terminal-1".to_owned(),
            scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                .with_owner_id(proof.owner_id())
                .with_channel("cli")
                .with_session_key(proof.session_key())
                .with_run_id(proof.run_id()),
            status: crate::agent::terminal::TurnTerminalStatus::Failed,
            history: None,
            history_scope: None,
            provider_outcome: None,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "local failure".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        },
        &config.cost,
        workspace.path(),
    )
    .await
    .expect_err("missing terminal decision must fail before persistence");
    assert_eq!(terminal.to_string(), "terminal draft has no service-issued decision");
    assert!(
        memory
            .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:unlinked-terminal-1:final")
            .await
            .expect("unlinked terminal marker lookup")
            .is_none(),
        "an unlinked terminal must not persist a marker"
    );
    let linked_commit = crate::agent::terminal::TurnTerminalDraft {
        decision: Some(
            service
                .settlement_ref(Arc::clone(&context))
                .expect("recordable service issues the terminal link"),
        ),
        terminal_id: "linked-terminal-1".to_owned(),
        scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
            .with_owner_id(proof.owner_id())
            .with_channel("cli")
            .with_session_key(proof.session_key())
            .with_run_id(proof.run_id()),
        status: crate::agent::terminal::TurnTerminalStatus::Failed,
        history: None,
        history_scope: None,
        provider_outcome: None,
        telemetry: crate::agent::terminal::TurnTerminalTelemetry {
            summary: "local failure".to_owned(),
            started_at: now,
            finished_at: now,
        },
        delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
    };
    let mut foreign = linked_commit.clone();
    foreign.terminal_id = "foreign-terminal-1".to_owned();
    foreign.scope.owner_id = Some("foreign_owner".to_owned());
    assert!(
        crate::agent::terminal::finalize_turn(&fabric, foreign, &config.cost, workspace.path())
            .await
            .is_err(),
        "a foreign terminal scope must fail before marker write"
    );
    assert!(
        memory
            .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:foreign-terminal-1:final")
            .await
            .expect("read foreign marker")
            .is_none()
    );
    let mut wrong_workspace = linked_commit.clone();
    wrong_workspace.terminal_id = "wrong-workspace-terminal-1".to_owned();
    assert!(
        crate::agent::terminal::finalize_turn(
            &fabric,
            wrong_workspace,
            &config.cost,
            workspace.path().join("other").as_path(),
        )
        .await
        .is_err(),
        "the cost ledger workspace must match before a terminal write"
    );
    assert!(
        memory
            .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:wrong-workspace-terminal-1:final")
            .await
            .expect("read workspace marker")
            .is_none()
    );
    let mut wrong_cost = config.cost.clone();
    wrong_cost.daily_limit_usd += 1.0;
    let mut wrong_tariff = linked_commit.clone();
    wrong_tariff.terminal_id = "wrong-cost-terminal-1".to_owned();
    assert!(
        crate::agent::terminal::finalize_turn(&fabric, wrong_tariff, &wrong_cost, workspace.path())
            .await
            .is_err(),
        "the cost configuration must match before a terminal write"
    );
    assert!(
        memory
            .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:wrong-cost-terminal-1:final")
            .await
            .expect("read cost marker")
            .is_none()
    );
    let linked = crate::agent::terminal::finalize_turn(&fabric, linked_commit.clone(), &config.cost, workspace.path())
        .await
        .expect("linked terminal after marker");
    let marker: serde_json::Value = serde_json::from_str(
        linked
            .terminal_event
            .raw_payload_json
            .as_deref()
            .expect("persisted marker payload"),
    )
    .expect("terminal marker JSON");
    assert_eq!(
        marker.get("decision_id").and_then(serde_json::Value::as_str),
        Some(context.decision_id().to_string().as_str())
    );
    assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let replay = crate::agent::terminal::finalize_turn(&fabric, linked_commit.clone(), &config.cost, workspace.path())
        .await
        .expect("same terminal replay");
    assert_eq!(replay.terminal_event.id, linked.terminal_event.id);
    assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let linked_scope = crate::causal_tree::store::DecisionScope::Owner {
        workspace_id: service.store.workspace_id().expect("store scope"),
        owner_id: Arc::from(proof.owner_id()),
    };
    let linked_row = service
        .store
        .reader()
        .get(&linked_scope, &context.decision_id())
        .expect("read linked decision")
        .expect("linked decision row");
    assert_eq!(linked_row.outcomes.len(), 1, "same terminal replay is idempotent");
    let outcome = linked_row.outcomes.first().expect("one linked outcome");
    assert_eq!(outcome.settlement_id, "linked-terminal-1");
    assert_eq!(outcome.classification, "failed");
    let conflicting_context = service
        .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
        .await
        .expect("second local decision");
    let mut conflicting_commit = linked_commit.clone();
    conflicting_commit.decision = Some(
        service
            .settlement_ref(Arc::clone(&conflicting_context))
            .expect("second decision is recordable"),
    );
    assert!(
        crate::agent::terminal::finalize_turn(&fabric, conflicting_commit, &config.cost, workspace.path())
            .await
            .is_err(),
        "a terminal ID cannot be reassigned to another decision"
    );
    assert_eq!(
        memory
            .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:linked-terminal-1:final")
            .await
            .expect("read retained marker")
            .expect("retained marker")
            .id,
        linked.terminal_event.id
    );
    assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let conflicting_scope = crate::causal_tree::store::DecisionScope::Owner {
        workspace_id: service.store.workspace_id().expect("store scope"),
        owner_id: Arc::from(proof.owner_id()),
    };
    assert!(
        service
            .store
            .reader()
            .get(&conflicting_scope, &conflicting_context.decision_id())
            .expect("read conflicting decision")
            .expect("conflicting decision row")
            .outcomes
            .is_empty(),
        "a conflicting replay must not write an outcome"
    );
    let mut concurrent_a = linked_commit.clone();
    concurrent_a.terminal_id = "concurrent-terminal-1".to_owned();
    let mut concurrent_b = concurrent_a.clone();
    concurrent_b.decision = Some(
        service
            .settlement_ref(Arc::clone(&conflicting_context))
            .expect("second concurrent decision link"),
    );
    let priced_outcome =
        |decision_id: DecisionId, prompt_tokens: u32| crate::llm::route_decision::ProviderExecutionOutcome {
            prepared_parameters: Vec::new(),
            decision_id: decision_id.to_string(),
            started_at: now,
            finished_at: now,
            attempts: Vec::new(),
            final_provider: "prx_test".to_owned(),
            final_model: "model".to_owned(),
            status: crate::llm::route_decision::ExecutionStatus::AllFailed {
                last_error_class: "local_failure".to_owned(),
            },
            fallback_reason: None,
            tokens_used: crate::llm::route_decision::TokenUsage {
                prompt_tokens: Some(prompt_tokens),
                total_tokens: Some(prompt_tokens),
                ..Default::default()
            },
        };
    concurrent_a.provider_outcome = Some(priced_outcome(context.decision_id(), 10));
    concurrent_b.provider_outcome = Some(priced_outcome(conflicting_context.decision_id(), 20));
    let (first, second) = tokio::join!(
        crate::agent::terminal::finalize_turn(&fabric, concurrent_a, &config.cost, workspace.path()),
        crate::agent::terminal::finalize_turn(&fabric, concurrent_b, &config.cost, workspace.path())
    );
    assert_ne!(
        first.is_ok(),
        second.is_ok(),
        "exactly one decision must claim the terminal ID"
    );
    let winner = first
        .as_ref()
        .ok()
        .or_else(|| second.as_ref().ok())
        .expect("one terminal winner");
    let claim = memory
        .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:concurrent-terminal-1:decision-claim")
        .await
        .expect("read terminal claim")
        .expect("terminal claim exists");
    let claimed: serde_json::Value =
        serde_json::from_str(claim.raw_payload_json.as_deref().expect("claim payload")).expect("claim JSON");
    let winner_marker: serde_json::Value = serde_json::from_str(
        winner
            .terminal_event
            .raw_payload_json
            .as_deref()
            .expect("winner marker payload"),
    )
    .expect("winner marker JSON");
    assert_eq!(claimed.get("decision_id"), winner_marker.get("decision_id"));
    let winner_tokens = winner_marker
        .pointer("/usage_settlement/prompt_tokens")
        .and_then(serde_json::Value::as_u64)
        .expect("winner metered tokens");
    let ledger = std::fs::read_to_string(workspace.path().join("state/costs.jsonl")).expect("persisted cost ledger");
    let concurrent_costs: Vec<serde_json::Value> = ledger
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row.get("settlement_id").and_then(serde_json::Value::as_str) == Some("concurrent-terminal-1"))
        .collect();
    assert_eq!(concurrent_costs.len(), 1, "the loser must not write a cost settlement");
    assert_eq!(
        concurrent_costs
            .first()
            .expect("one concurrent cost settlement")
            .pointer("/usage/input_tokens")
            .and_then(serde_json::Value::as_u64),
        Some(winner_tokens),
        "the cost ledger must belong to the claimed winner"
    );
    assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let concurrent_count = [context.decision_id(), conflicting_context.decision_id()]
        .into_iter()
        .map(|id| {
            service
                .store
                .reader()
                .get(&conflicting_scope, &id)
                .expect("read concurrent decision")
                .expect("concurrent decision row")
                .outcomes
                .iter()
                .filter(|outcome| outcome.settlement_id == "concurrent-terminal-1")
                .count()
        })
        .sum::<usize>();
    assert_eq!(concurrent_count, 1, "the losing decision cannot settle an outcome");
    let mut divergent_a = linked_commit.clone();
    divergent_a.terminal_id = "divergent-terminal-1".to_owned();
    divergent_a.provider_outcome = Some(priced_outcome(context.decision_id(), 30));
    let mut divergent_b = divergent_a.clone();
    divergent_b.provider_outcome = Some(priced_outcome(context.decision_id(), 40));
    let (first, second) = tokio::join!(
        crate::agent::terminal::finalize_turn(&fabric, divergent_a, &config.cost, workspace.path()),
        crate::agent::terminal::finalize_turn(&fabric, divergent_b, &config.cost, workspace.path())
    );
    assert_ne!(
        first.is_ok(),
        second.is_ok(),
        "divergent facts cannot share a decision claim"
    );
    let winner = first
        .as_ref()
        .ok()
        .or_else(|| second.as_ref().ok())
        .expect("one semantic winner");
    let winner_marker: serde_json::Value = serde_json::from_str(
        winner
            .terminal_event
            .raw_payload_json
            .as_deref()
            .expect("semantic winner marker"),
    )
    .expect("semantic winner JSON");
    let winner_tokens = winner_marker
        .pointer("/usage_settlement/prompt_tokens")
        .and_then(serde_json::Value::as_u64)
        .expect("semantic winner tokens");
    let ledger = std::fs::read_to_string(workspace.path().join("state/costs.jsonl")).expect("semantic cost ledger");
    let divergent_costs: Vec<serde_json::Value> = ledger
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row.get("settlement_id").and_then(serde_json::Value::as_str) == Some("divergent-terminal-1"))
        .collect();
    assert_eq!(divergent_costs.len(), 1);
    assert_eq!(
        divergent_costs
            .first()
            .expect("one semantic cost settlement")
            .pointer("/usage/input_tokens")
            .and_then(serde_json::Value::as_u64),
        Some(winner_tokens),
        "only the semantic claim winner can write cost"
    );
    let mut scope_commit = linked_commit.clone();
    scope_commit.terminal_id = "scope-replay-terminal-1".to_owned();
    scope_commit.provider_outcome = Some(priced_outcome(context.decision_id(), 50));
    let scope_marker =
        crate::agent::terminal::finalize_turn(&fabric, scope_commit.clone(), &config.cost, workspace.path())
            .await
            .expect("claimed terminal before scope replay");
    let mut changed_scope = scope_commit;
    changed_scope.scope.recipient = Some("prx_other_recipient".to_owned());
    assert!(
        crate::agent::terminal::finalize_turn(&fabric, changed_scope, &config.cost, workspace.path())
            .await
            .is_err(),
        "same decision with changed persisted scope must fail before cost"
    );
    assert_eq!(
        memory
            .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:scope-replay-terminal-1:final")
            .await
            .expect("read scope replay marker")
            .expect("retained scope marker")
            .id,
        scope_marker.terminal_event.id
    );
    let scope_ledger =
        std::fs::read_to_string(workspace.path().join("state/costs.jsonl")).expect("scope replay cost ledger");
    assert_eq!(
        scope_ledger
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|row| {
                row.get("settlement_id").and_then(serde_json::Value::as_str) == Some("scope-replay-terminal-1")
            })
            .count(),
        1,
        "changed scope cannot add a cost settlement"
    );
    let rejoin_context = service
        .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
        .await
        .expect("decision before terminal-only crash point");
    assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let rejoin_marker = crate::agent::terminal::finalize_turn_stop_after_marker(
        &fabric,
        crate::agent::terminal::TurnTerminalDraft {
            decision: Some(
                service
                    .settlement_ref(Arc::clone(&rejoin_context))
                    .expect("recordable link"),
            ),
            terminal_id: "rejoin-terminal-1".to_owned(),
            scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                .with_owner_id(proof.owner_id())
                .with_channel("cli")
                .with_session_key(proof.session_key())
                .with_run_id(proof.run_id()),
            status: crate::agent::terminal::TurnTerminalStatus::Failed,
            history: None,
            history_scope: None,
            provider_outcome: None,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "local failure".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        },
        &config.cost,
        workspace.path(),
    )
    .await
    .expect("real finalizer marker before outcome admission");
    let rejoin_scope = crate::causal_tree::store::DecisionScope::Owner {
        workspace_id: service.store.workspace_id().expect("store scope"),
        owner_id: Arc::from(proof.owner_id()),
    };
    assert!(
        service
            .store
            .reader()
            .get(&rejoin_scope, &rejoin_context.decision_id())
            .expect("read pending decision")
            .expect("pending decision row")
            .outcomes
            .is_empty(),
        "expected an empty value"
    );
    let late_context = service
        .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
        .await
        .expect("decision for delayed terminal task");
    let late_commit = crate::agent::terminal::TurnTerminalDraft {
        decision: Some(
            service
                .settlement_ref(Arc::clone(&late_context))
                .expect("delayed decision link"),
        ),
        terminal_id: "late-async-terminal-1".to_owned(),
        scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
            .with_owner_id(proof.owner_id())
            .with_channel("cli")
            .with_session_key(proof.session_key())
            .with_run_id(proof.run_id()),
        status: crate::agent::terminal::TurnTerminalStatus::Failed,
        history: None,
        history_scope: None,
        provider_outcome: None,
        telemetry: crate::agent::terminal::TurnTerminalTelemetry {
            summary: "delayed local failure".to_owned(),
            started_at: now,
            finished_at: now,
        },
        delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
    };
    let (release_late, await_late) = tokio::sync::oneshot::channel::<()>();
    let late_fabric = fabric.clone();
    let late_cost = config.cost.clone();
    let late_workspace = workspace.path().to_path_buf();
    let late_task = tokio::spawn(async move {
        await_late.await.expect("release delayed terminal task");
        crate::agent::terminal::finalize_turn(&late_fabric, late_commit, &late_cost, &late_workspace).await
    });
    assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
    assert!(
        service
            .store
            .reader()
            .get(&rejoin_scope, &late_context.decision_id())
            .expect("read delayed decision")
            .expect("delayed decision row")
            .outcomes
            .is_empty(),
        "the delayed task cannot settle before its terminal marker"
    );
    release_late.send(()).expect("release delayed terminal task");
    let late_receipt = late_task
        .await
        .expect("join delayed terminal task")
        .expect("late terminal join");
    assert_eq!(late_receipt.terminal_event.event_type, "turn.finalized");
    assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let late_row = service
        .store
        .reader()
        .get(&rejoin_scope, &late_context.decision_id())
        .expect("read settled delayed decision")
        .expect("settled delayed decision row");
    assert_eq!(late_row.outcomes.len(), 1);
    assert_eq!(
        late_row.outcomes.first().expect("one late outcome").settlement_id,
        "late-async-terminal-1"
    );
    assert_eq!(
        late_row.outcomes.first().expect("one late outcome").classification,
        "failed"
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    assert_eq!(
        service.store.reader().counts().expect("counts").decisions,
        i64::try_from(
            context.hop_ids().len()
                + conflicting_context.hop_ids().len()
                + rejoin_context.hop_ids().len()
                + late_context.hop_ids().len()
        )
        .expect("bounded hop count")
            + 5
    );
    let scope = crate::causal_tree::store::DecisionScope::Owner {
        workspace_id: service.store.workspace_id().expect("store scope"),
        owner_id: Arc::from(proof.owner_id()),
    };
    for (index, hop) in context.audit_hops().enumerate() {
        let row = service
            .store
            .reader()
            .get(&scope, &hop.decision_id)
            .expect("read")
            .expect("recorded hop");
        assert_eq!(row.decision_id, hop.decision_id);
        assert_eq!(
            row.prior_decision_id,
            index
                .checked_sub(1)
                .and_then(|prior| context.hop_ids().get(prior))
                .copied()
        );
        assert_eq!(row.run_id.as_deref(), Some(proof.run_id()));
        assert!(row.enabled_snapshot);
        assert!(
            row.selected_candidate_id
                .as_ref()
                .is_some_and(|selected| { row.candidates.iter().any(|candidate| &candidate.id == selected) })
        );
    }
    assert_eq!(transports.0.load(Ordering::SeqCst), 0, "no external transport");
    let old_store = Arc::clone(&service.store);
    drop(linked_commit);
    drop(writer);
    drop(service);
    CausalDecisionStore::close(old_store, Instant::now() + Duration::from_secs(5)).await;
    let reopened_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let reopened_security = Arc::new(SecurityPolicy::default());
    let reopened_execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&reopened_registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&reopened_security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let reopened = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry: reopened_registry,
            execution: reopened_execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security: reopened_security,
        },
    )
    .await
    .expect("reopened recordable service");
    // Reconstructed services read their own committed source marker.
    // There is no caller backend or workspace parameter to substitute.
    assert_eq!(
        reopened
            .rejoin_terminal("rejoin-terminal-1", proof.owner_id())
            .await
            .expect("rejoin reads the service-owned persisted marker"),
        Enqueue::Queued
    );
    assert!(
        reopened
            .rejoin_terminal("rejoin-terminal-1", "foreign_owner")
            .await
            .is_err(),
        "a foreign owner cannot rejoin the outcome"
    );
    for (terminal, owner) in [
        ("prx_missing_terminal", proof.owner_id()),
        ("", proof.owner_id()),
        ("rejoin-terminal-1", ""),
    ] {
        assert!(reopened.rejoin_terminal(terminal, owner).await.is_err());
    }
    let foreign_workspace = tempfile::tempdir().expect("foreign workspace");
    let mut foreign_config = config.clone();
    foreign_config.workspace_dir = foreign_workspace.path().to_path_buf();
    let foreign_generation = crate::config::new_shared(foreign_config.clone()).pin();
    let foreign_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let foreign_security = Arc::new(SecurityPolicy::default());
    let foreign_execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&foreign_registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&foreign_security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let foreign_service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&foreign_generation),
            registry: foreign_registry,
            execution: foreign_execution,
            catalog_sources: sources(&foreign_generation, &foreign_config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::new(crate::memory::SqliteMemory::new(foreign_workspace.path()).expect("foreign SQLite")),
            security: foreign_security,
        },
    )
    .await
    .expect("foreign recordable service");
    assert!(
        foreign_service
            .rejoin_terminal("rejoin-terminal-1", proof.owner_id())
            .await
            .is_err(),
        "a different workspace cannot read or settle this service's marker"
    );
    foreign_service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    for _ in 0..2 {
        assert_eq!(
            reopened
                .rejoin_terminal("rejoin-terminal-1", proof.owner_id())
                .await
                .expect("rejoin from persisted marker and decision"),
            Enqueue::Queued
        );
    }
    let reopened_writer = reopened.store.writer().expect("reopened writer");
    assert!(reopened_writer.drain(Instant::now() + Duration::from_secs(5)).await);
    assert_eq!(
        reopened
            .store
            .reader()
            .get(&rejoin_scope, &rejoin_context.decision_id())
            .expect("read rejoined decision")
            .expect("rejoined decision row")
            .outcomes
            .len(),
        1
    );
    assert_eq!(
        memory
            .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:rejoin-terminal-1:final")
            .await
            .expect("read retained rejoin marker")
            .expect("retained rejoin marker")
            .id,
        rejoin_marker.terminal_event.id
    );
    let completed_context = reopened
        .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
        .await
        .expect("completed decision after service reopen");
    let (expected_classification, completed_outcome) = match completed_context.terminal_outcome_action() {
        Some(crate::causal_tree::branch::CausalActionKind::Clarify) => ("clarification", None),
        Some(crate::causal_tree::branch::CausalActionKind::Refuse) => ("refusal", None),
        Some(crate::causal_tree::branch::CausalActionKind::Answer) => (
            "success",
            Some(crate::llm::route_decision::ProviderExecutionOutcome {
                prepared_parameters: Vec::new(),
                decision_id: completed_context.decision_id().to_string(),
                started_at: now,
                finished_at: now,
                attempts: Vec::new(),
                final_provider: "prx_test".to_owned(),
                final_model: "prx_test".to_owned(),
                status: crate::llm::route_decision::ExecutionStatus::Success,
                fallback_reason: None,
                tokens_used: Default::default(),
            }),
        ),
        _ => panic!("the selected terminal action is unsupported by CDP-23"),
    };
    assert!(reopened_writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let completed_marker = crate::agent::terminal::finalize_turn_stop_after_marker(
        &fabric,
        crate::agent::terminal::TurnTerminalDraft {
            decision: Some(
                reopened
                    .settlement_ref(Arc::clone(&completed_context))
                    .expect("completed link"),
            ),
            terminal_id: "completed-rejoin-terminal-1".to_owned(),
            scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                .with_owner_id(proof.owner_id())
                .with_channel("cli")
                .with_session_key(proof.session_key())
                .with_run_id(proof.run_id()),
            status: crate::agent::terminal::TurnTerminalStatus::Completed,
            history: None,
            history_scope: None,
            provider_outcome: completed_outcome,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "local completion".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        },
        &config.cost,
        workspace.path(),
    )
    .await
    .expect("completed marker before outcome admission");
    assert_eq!(
        reopened
            .rejoin_terminal("completed-rejoin-terminal-1", proof.owner_id())
            .await
            .expect("completed rejoin from persisted selected action"),
        Enqueue::Queued
    );
    assert!(reopened_writer.drain(Instant::now() + Duration::from_secs(5)).await);
    let completed_row = reopened
        .store
        .reader()
        .get(&rejoin_scope, &completed_context.decision_id())
        .expect("read completed decision")
        .expect("completed decision row");
    assert_eq!(completed_row.outcomes.len(), 1);
    assert_eq!(
        completed_row
            .outcomes
            .first()
            .expect("one completed outcome")
            .classification,
        expected_classification
    );
    assert_eq!(
        memory
            .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:completed-rejoin-terminal-1:final")
            .await
            .expect("read completed marker")
            .expect("completed marker")
            .id,
        completed_marker.terminal_event.id
    );
}

#[cfg(unix)]
#[tokio::test]
async fn disabled_service_ignores_an_unsafe_store_path_without_touching_it() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().expect("test workspace");
    let outside = tempfile::tempdir().expect("test outside directory");
    std::fs::create_dir(workspace.path().join("self")).expect("test self directory");
    symlink(outside.path(), workspace.path().join("self/causal_tree")).expect("test unsafe store path");
    let mut config = crate::config::Config::default();
    config.causal_tree.enabled = false;
    config.workspace_dir = workspace.path().to_path_buf();
    let generation = crate::config::new_shared(config.clone()).pin();
    let service = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config)).await;
    assert_eq!(service.store.mode(), StoreMode::ReadOnly);
    assert!(!service.recordable);
    assert!(
        std::fs::read_dir(outside.path())
            .expect("outside directory remains readable")
            .next()
            .is_none()
    );
}
