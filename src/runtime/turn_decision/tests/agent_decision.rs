#![cfg(test)]
use super::*;

#[tokio::test]
async fn record_builder_records_the_prediction_free_opening_and_rejects_a_broken_chain() {
    use crate::causal_tree::outcome::{DecisionRecordBuilder, RecordBuildError};

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
        "neutral request",
    );
    let mut issued = service
        .open_issued_turn_context(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
        .await
        .expect("test: prediction-free opening");
    let context = Arc::get_mut(&mut issued).expect("test: sole context owner");
    let key = crate::causal_tree::state::fixtures::key();
    assert!(matches!(
        DecisionRecordBuilder::build_enabled_chain(context, &key, false),
        Err(RecordBuildError::Binding)
    ));
    let local = DecisionRecordBuilder::build_enabled_chain(context, &key, true).expect("local terminal record");
    assert_eq!(local.len(), 1);
    let local_row = local.first().expect("one local terminal");
    let crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. } = &context.engine.selection;
    assert_eq!(local_row.backend_kind, "none");
    assert_eq!(local_row.selected_candidate_id.as_deref(), Some(candidate_id.as_str()));
    assert_eq!(local_row.protocol_version, 0);
    assert_eq!(local_row.error_class, None);
    context.hop_ids.clear();
    assert!(matches!(
        DecisionRecordBuilder::build_enabled_chain(context, &key, true),
        Err(RecordBuildError::Chain)
    ));
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
    assert_eq!(snapshot.budgets.tokens, 3_584);
    assert_eq!(snapshot.budgets.micro_usd, 500_000);
    assert_eq!(
        snapshot.budgets.latency_ms,
        config.causal_tree.policy.extra_latency_budget_ms
    );
    assert_eq!(snapshot.main_compaction.max_context_tokens, 32_768);
    assert_eq!(snapshot.main_compaction.reserve_tokens, 7_680);
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
