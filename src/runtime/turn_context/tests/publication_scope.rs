//! The publication-scoped model, budget and route owner (`PublicationScope`)
//! read through a fixture turn service.
use crate::providers::catalog::CatalogView as ProviderCatalogView;
use crate::runtime::turn_publication::service::test_support::disabled_agent_service;
use crate::runtime::turn_publication::service::{AgentDecisionBudgetError, TurnModelError};

#[tokio::test]
async fn agent_budget_uses_pinned_route_and_read_only_spend_snapshot() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
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
    assert_eq!(snapshot.budgets.latency_ms, config.decision_engine.latency_budget_ms);
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

#[tokio::test]
async fn agent_hint_route_drift_is_refused_before_credential_resolution() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        api_key: None,
        credential_ref: None,
    });
    let generation = crate::config::new_shared(config.clone()).pin();
    let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "hint:fast");
    let matching = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config)).await;
    assert!(matching.resolve_agent_model(&proof).is_ok());
    assert!(crate::runtime::turn_model::validate_execution(&proof, "ollama", "hint:fast").is_ok());
    assert!(crate::runtime::turn_model::validate_execution(&proof, "openai", "gpt-4o-mini").is_ok());
    for (provider, model) in [("ollama", "gpt-4o-mini"), ("openai", "hint:fast"), ("openai", "gpt-4o")] {
        assert!(
            crate::runtime::turn_model::validate_execution(&proof, provider, model).is_err(),
            "test: mismatched execution model must be refused by the ingress binding"
        );
    }

    let mut foreign = config.clone();
    foreign.model_routes.first_mut().expect("configured route").model = "gpt-4o".to_string();
    let stale = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign)).await;
    assert!(matches!(
        stale.resolve_agent_model(&proof),
        Err(TurnModelError::RouteDrift)
    ));

    assert!(!stale.model_can_call_tools(&proof));

    let route = foreign.model_routes.first_mut().expect("configured route");
    route.model = "gpt-4o-mini".to_string();
    route.credential_ref = Some("cred-other".to_string());
    let wrong_owner = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign)).await;
    assert!(matches!(
        wrong_owner.resolve_agent_model(&proof),
        Err(TurnModelError::RouteDrift)
    ));
    assert!(!wrong_owner.model_can_call_tools(&proof));
}

#[tokio::test]
async fn verified_native_tools_requires_matching_source_selection() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config
        .providers
        .models
        .push(crate::config::schema::ProviderModelConfig {
            provider: "ollama".to_string(),
            model: Some("prx-local-model".to_string()),
            operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
            native_tools: Some(true),
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema_dialect: None,
            tool_call_dialect: None,
            context_window: None,
        });
    let generation = crate::config::new_shared(config.clone()).pin();
    let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "prx-local-model");
    let service = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config)).await;
    assert!(service.resolve_agent_model(&proof).is_ok());
    assert!(service.model_can_call_tools(&proof));

    let mut foreign = config;
    foreign
        .providers
        .models
        .first_mut()
        .expect("configured model")
        .native_tools = Some(false);
    let stale = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign)).await;
    assert!(!stale.model_can_call_tools(&proof));
}
