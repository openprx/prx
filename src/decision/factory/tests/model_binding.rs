#![cfg(test)]
//! Model binding at the real configured factory and HTTP adapter boundary.
//! This is component evidence, not whole-service budget admission.
#![allow(clippy::expect_used)]
use super::*;
use crate::decision::RequestRule;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct Capture {
    sends: Arc<AtomicUsize>,
    bodies: Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
}
async fn capture_failure(
    axum::extract::State(state): axum::extract::State<Capture>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::http::StatusCode {
    state.sends.fetch_add(1, Ordering::SeqCst);
    state.bodies.lock().push(body);
    axum::http::StatusCode::SERVICE_UNAVAILABLE
}

#[tokio::test]
#[allow(clippy::print_stdout)]
async fn configured_http_wrong_model_is_refused_before_send() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("owned loopback listener");
    let endpoint = format!("http://{}", listener.local_addr().expect("actual loopback address"));
    let capture = Capture {
        sends: Arc::new(AtomicUsize::new(0)),
        bodies: Arc::new(parking_lot::Mutex::new(Vec::new())),
    };
    let router = axum::Router::new()
        .route("/v1/systemone", axum::routing::post(capture_failure))
        .with_state(capture.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("owned loopback server");
    });
    let workspace = tempfile::tempdir().expect("isolated workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.config_path = workspace.path().join("config.toml");
    config.causal_tree.enabled = true;
    config.causal_tree.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
    config.causal_tree.backend.endpoint = endpoint.clone();
    config.causal_tree.backend.model = "prx-decision-model".to_owned();
    config.causal_tree.backend.timeout_ms = 1000;
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config
        .causal_tree
        .validate()
        .expect("legal configured decision fixture");
    config.providers.models.push(serde_json::from_value(serde_json::json!({"provider":format!("custom:{endpoint}"),"model":"prx-decision-model","operations":["decision_endpoint"]})).expect("declared actual decision model"));
    let catalog = CatalogView::from_config(&config);
    let key = DecisionKey::load_or_create(workspace.path()).expect("owned fixture key");
    let mut factory = DecisionBackendFactory::from_config(
        &config,
        FactoryDeps {
            catalog: &catalog,
            key: Some(Arc::clone(&key)),
            transports: &crate::decision::compatible_http::HttpDecisionTransports,
            config_generation: 7,
            events: Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
        },
    )
    .expect("genuine configured factory with actual HTTP adapter");
    assert_eq!(factory.status().order.len(), 1);
    assert_eq!(factory.status().order[0].locality, BackendLocality::Loopback);
    let mut questions = BTreeMap::new();
    questions.insert(
        QuestionId::global(GlobalQuestion::EvidenceSufficient),
        DecisionQuestion::Noul {
            instructions: StructuredValue::try_from(serde_json::json!("Is neutral project evidence sufficient?"))
                .expect("valid instruction"),
            criteria: None,
        },
    );
    let request = DecisionRequest::seal(
        RequestParts {
            state: StructuredValue::try_from(serde_json::json!({"goal":"neutral project evidence"}))
                .expect("neutral state"),
            questions,
            model: Some("prx-main-model".to_owned()),
            state_fingerprint: "fixture-state".to_owned(),
            candidate_set_hash: "fixture-candidates".to_owned(),
            policy_version: Arc::from("fixture-policy"),
            catalog_generation: "fixture-catalog".to_owned(),
            omitted_questions: Vec::new(),
            approval_pending: false,
        },
        &key,
        Instant::now() + Duration::from_secs(5),
        &RequestLimits::default(),
    )
    .expect("valid but slot-mismatched sealed request");
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("safe builtin fallback");
    // A contradictory slot must also be denied on the actual HTTP health path.
    factory.slots.first_mut().expect("actual owned slot").request_model = Arc::from("prx-other-model");
    factory.check_health().await;
    assert!(!factory.status().order.first().expect("actual slot").healthy);
    server.abort();
    assert!(server.await.expect_err("owned server cancelled").is_cancelled());
    let sends = capture.sends.load(Ordering::SeqCst);
    println!(
        "PRX_CDP_MODEL_BINDING {}",
        serde_json::json!({"kind":"configured-http-wrong-model-component","sends":sends,"captured":*capture.bodies.lock(),"request_model":request.model(),"configured_model":config.causal_tree.backend.model})
    );
    assert_eq!(
        sends, 0,
        "a slot-mismatched sealed model must be refused before real HTTP I/O"
    );
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert_eq!(result.attempts.len(), 1);
    assert_eq!(
        result.attempts[0].outcome,
        AttemptOutcome::Skipped(DecisionError::InvalidRequest(RequestRule::ModelInvalid))
    );
}

#[tokio::test]
async fn mismatched_model_ignores_a_primed_cache() {
    let key = DecisionKey::ephemeral();
    let request = request(&key);
    let (backend, mock) = scripted(&key, "prx_cache_route", Vec::new());
    let mut factory = DecisionBackendFactory::from_slots(
        vec![backend],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let identity = factory.slots.first().expect("owned slot").backend.identity().clone();
    let cached = DecisionResponseValidator::validate(
        &request,
        answer_as(&request, "prx_cache_route"),
        &ValidationContext {
            received_at: Instant::now(),
            max_response_bytes: u32::MAX,
            version_policy: ModelVersionPolicy::Unpinned,
            local_backend: identity.clone(),
        },
    )
    .expect("valid cache fixture");
    factory.cache.put(
        CacheKey::new(&request, &identity, factory.config_generation),
        cached,
        Instant::now(),
    );
    factory.slots.first_mut().expect("owned slot").request_model = Arc::from("prx-other-model");
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("safe fallback");
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert_eq!(
        result.attempts.first().expect("refused slot").outcome,
        AttemptOutcome::Skipped(DecisionError::InvalidRequest(RequestRule::ModelInvalid))
    );
    assert_eq!(mock.calls(), 0);
    assert_eq!(
        factory.cache.len(),
        1,
        "the invalid request did not consume the primed cache"
    );
    factory.check_health().await;
    assert!(!factory.status().order.first().expect("slot status").healthy);
    assert_eq!(mock.calls(), 0, "contradictory slot model cannot reach health I/O");
}

#[test]
fn request_model_only_comes_from_coherent_retained_slots() {
    let key = DecisionKey::ephemeral();
    let (backend, _) = scripted(&key, "prx_route_a", Vec::new());
    let factory = DecisionBackendFactory::from_slots(
        vec![backend],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    assert_eq!(factory.request_model(), Some(MODEL));
    factory
        .slots
        .first()
        .expect("slot")
        .healthy
        .store(false, Ordering::Relaxed);
    assert_eq!(
        factory.request_model(),
        Some(MODEL),
        "temporary health must not change the sealed model"
    );
    assert_eq!(factory.with_unproven_budget_gate().request_model(), None);
    let (first, _) = scripted(&key, "prx_route_a", Vec::new());
    let (second, _) = scripted(&key, "prx_route_b", Vec::new());
    let mut divergent = DecisionBackendFactory::from_slots(
        vec![first, second],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    divergent.slots.get_mut(1).expect("second slot").request_model = Arc::from("prx-other-model");
    assert_eq!(divergent.request_model(), None);
    let empty = DecisionBackendFactory::from_slots(Vec::new(), key, OutboundSecrets::default(), SlotLimits::default());
    assert_eq!(empty.request_model(), None);
}

#[test]
fn configured_adapter_cannot_replace_the_selected_model() {
    struct Contradictory;
    impl DecisionTransports for Contradictory {
        fn build(&self, spec: &BackendSpec<'_>) -> Result<Box<dyn DecisionBackend>, DecisionError> {
            let mut identity = spec.identity.clone();
            identity.configured_model = Arc::from("prx-other-model");
            Ok(Box::new(Scripted {
                mock: Arc::new(MockDecisionBackend::new(Arc::clone(&spec.key), Vec::new())),
                identity,
                received: Received::default(),
            }))
        }
    }
    let workspace = tempfile::tempdir().expect("owned workspace");
    let config = config(&workspace, |section| {
        section.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        section.backend.endpoint = LOOPBACK.to_owned();
        section.backend.model = MODEL.to_owned();
    });
    let catalog = catalog(&config, &format!("custom:{LOOPBACK}"));
    let result = DecisionBackendFactory::from_config(
        &config,
        FactoryDeps {
            catalog: &catalog,
            key: Some(key(&workspace)),
            transports: &Contradictory,
            config_generation: 7,
            events: Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
        },
    );
    assert!(matches!(
        result,
        Err(FactoryBuildError::Transport(DecisionError::InvalidRequest(
            RequestRule::ModelInvalid
        )))
    ));
}
