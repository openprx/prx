#![cfg(test)]

use super::*;
use crate::config::manage::mutation::decision_engine::{CausalConfigService, CausalMutationError};
use crate::config::schema::decision_engine::{BackendSelection, effective_engine_backend};

/// Audit r1 F6 follow-on: the hosted choice the onboarding wizard builds for
/// OpenRouter `typesafe/jev-1.13` passes its pre-write check and writes
/// `latency_budget_ms` = the catalog recommendation (3000) explicitly, in the
/// same transaction as the backend, over a lower written budget. The plain
/// mutation API keeps refusing the same selection.
#[tokio::test]
async fn wizard_hosted_openrouter_writes_the_recommended_latency_budget() {
    let temp = tempfile::TempDir::new().expect("test: temp");
    let mut config = Config {
        config_path: temp.path().join("config.toml"),
        workspace_dir: temp.path().join("workspace"),
        ..Config::default()
    };
    config.secrets.encrypt = false;
    crate::config::files::write_split_config(&config, false)
        .await
        .expect("test: write base configuration");
    let written = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load");
    assert_eq!(written.decision_engine.latency_budget_ms, 500);

    // The setup `setup_decision_plane` builds for "a hosted service",
    // OpenRouter, model `typesafe/jev-1.13`, engine left off.
    let setup = DecisionSetup {
        kind: BackendKind::OpenRouter,
        endpoint: None,
        model: Some("typesafe/jev-1.13".to_string()),
        enable: false,
    };
    setup
        .check_hosted()
        .expect("test: the wizard's hosted choice passes its pre-write check");
    setup
        .apply_hosted(
            written,
            crate::config::manage::credentials::SecretString::new("sk-WIZARD-SENTINEL-0001".to_string()),
        )
        .await
        .expect("test: the wizard's hosted choice applies");

    let reloaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: reload");
    assert_eq!(reloaded.decision_engine.latency_budget_ms, 3_000);
    let backend = effective_engine_backend(&reloaded);
    assert!(matches!(backend.kind, BackendSelection::Fixed(BackendKind::OpenRouter)));
    assert_eq!(backend.model, "typesafe/jev-1.13");

    let refused = CausalConfigService::mutate(
        &Config::default(),
        &[CausalMutation::SetBackend {
            kind: BackendKind::OpenRouter,
            endpoint: None,
            model: Some("typesafe/jev-1.13".to_string()),
            pinned_snapshot: None,
            timeout_ms: None,
        }],
    );
    assert!(matches!(
        refused,
        Err(CausalMutationError::LatencyBudgetBelowRecommendation {
            configured: 500,
            recommended: 3_000,
            ..
        })
    ));
}
