//! External callers cannot fund physical decision IO without a real sealed owner.
//! Original16 transport oracles are mapped to private `causal_cli::backend_transport_tests`.
#![allow(clippy::expect_used, clippy::unwrap_used)]
use openprx::decision::factory::{AttemptOutcome, DecisionBackendFactory, FactoryDeps, ServedBy};
use openprx::decision::telemetry::{CausalEvent, CausalEventSink, CausalLineage};
use openprx::decision::{
    DecisionError, DecisionKey, DecisionQuestion, DecisionRequest, GlobalQuestion, QuestionId, RequestLimits,
    RequestParts, StructuredValue,
};
use openprx::providers::{
    ProviderId,
    catalog::{CatalogView, CuratedModel, ModelOperation},
};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
struct NoEvents;
impl CausalEventSink for NoEvents {
    fn emit(&self, _: CausalEvent) {}
}
struct Plane {
    _home: tempfile::TempDir,
    key: Arc<DecisionKey>,
    factory: DecisionBackendFactory,
    listener: tokio::net::TcpListener,
}
async fn plane() -> Plane {
    let home = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut config = openprx::config::Config {
        workspace_dir: home.path().to_owned(),
        config_path: home.path().join("config.toml"),
        ..openprx::config::Config::default()
    };
    config.causal_tree.enabled = true;
    config.causal_tree.backend.kind = serde_json::from_value(serde_json::json!("compatible_http")).unwrap();
    config.causal_tree.backend.endpoint.clone_from(&endpoint);
    "jev-1.13.0".clone_into(&mut config.causal_tree.backend.model);
    config.causal_tree.backend.remote_enabled = false;
    let catalog = CatalogView::from_config(&config).with_curated([CuratedModel {
        operations: Some(vec![ModelOperation::DecisionEndpoint]),
        ..CuratedModel::new(ProviderId::of(&format!("custom:{endpoint}")), Some("jev-1.13.0"))
    }]);
    let key = FactoryDeps::load_key(&config).unwrap();
    let factory = DecisionBackendFactory::from_config(
        &config,
        FactoryDeps {
            catalog: &catalog,
            key: Some(Arc::clone(&key)),
            transports: &openprx::decision::compatible_http::HttpDecisionTransports,
            config_generation: 1,
            events: Arc::new(NoEvents),
        },
    )
    .unwrap();
    Plane {
        _home: home,
        key,
        factory,
        listener,
    }
}
fn request(key: &DecisionKey) -> DecisionRequest {
    let mut questions = BTreeMap::new();
    questions.insert(
        QuestionId::global(GlobalQuestion::EvidenceSufficient),
        DecisionQuestion::Noul {
            instructions: StructuredValue::try_from(serde_json::json!("Is the evidence sufficient?")).unwrap(),
            criteria: None,
        },
    );
    DecisionRequest::seal(
        RequestParts {
            state: StructuredValue::try_from(serde_json::json!({"goal":"list files"})).unwrap(),
            questions,
            model: Some("jev-1.13.0".to_owned()),
            state_fingerprint: "state".to_owned(),
            candidate_set_hash: "candidates".to_owned(),
            policy_version: Arc::from("policy"),
            catalog_generation: "catalog".to_owned(),
            omitted_questions: Vec::new(),
            approval_pending: true,
        },
        key,
        Instant::now() + Duration::from_secs(5),
        &RequestLimits::default(),
    )
    .unwrap()
}
#[tokio::test]
async fn configured_external_decision_without_owner_is_refused_before_socket() {
    let p = plane().await;
    let result = p
        .factory
        .decide(&request(&p.key), &CausalLineage::default())
        .await
        .unwrap();
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert!(matches!(
        result.attempts.first().map(|a| &a.outcome),
        Some(AttemptOutcome::Failed(DecisionError::BudgetExceeded))
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), p.listener.accept())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn explicit_missing_owner_cannot_fund_external_health_probe() {
    let p = plane().await;
    let before = p.factory.status().order.first().unwrap().healthy;
    assert!(!p.factory.check_health_with_accounting(None).await);
    assert_eq!(p.factory.status().order.first().unwrap().healthy, before);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), p.listener.accept())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn legacy_external_health_wrapper_does_not_send_or_claim_new_health() {
    let p = plane().await;
    let before = p.factory.status().order.first().unwrap().healthy;
    p.factory.check_health().await;
    assert_eq!(p.factory.status().order.first().unwrap().healthy, before);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), p.listener.accept())
            .await
            .is_err()
    );
}
