#![cfg(test)]
//! Real compiler and configured HTTP component over an actual admitted service hop.
#![allow(clippy::expect_used)]
use super::*;
use crate::causal_tree::questions::{CompilerPolicy, QuestionCompiler};
use crate::decision::factory::{DecisionBackendFactory, FactoryDeps, ServedBy};

#[derive(Clone)]
struct Capture(Arc<parking_lot::Mutex<Vec<Value>>>);

#[allow(clippy::panic)] // Unknown primitives invalidate this owned protocol fixture.
async fn answer(
    axum::extract::State(capture): axum::extract::State<Capture>,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    capture.0.lock().push(body.clone());
    let answers: serde_json::Map<String, Value> = body
        .get("questions")
        .and_then(Value::as_object)
        .expect("actual wire questions")
        .iter()
        .map(|(id, q)| {
            let answer = match q.get("type").and_then(Value::as_str).expect("actual primitive") {
                "noul" => json!({"type":"noul","noul":0.5}),
                "choice" => {
                    let choices = q.get("criteria").and_then(Value::as_object).expect("choice options");
                    let first = choices.keys().next().expect("nonempty options");
                    let probabilities: serde_json::Map<String, Value> = choices
                        .keys()
                        .map(|name| (name.clone(), json!(if name == first { 1.0 } else { 0.0 })))
                        .collect();
                    json!({"type":"choice","choice":first,"probabilities":probabilities,"confidence":1.0})
                }
                "score" => {
                    let levels = q.get("criteria").and_then(Value::as_array).expect("score levels");
                    let legend: serde_json::Map<String, Value> = levels
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (i.to_string(), v.clone()))
                        .collect();
                    let probabilities: serde_json::Map<String, Value> = levels
                        .iter()
                        .enumerate()
                        .map(|(i, _)| (i.to_string(), json!(if i == 0 { 1.0 } else { 0.0 })))
                        .collect();
                    json!({"type":"score","score":0.0,"legend":legend,"probabilities":probabilities,"confidence":1.0})
                }
                other => panic!("unexpected actual primitive {other}"),
            };
            (id.clone(), answer)
        })
        .collect();
    axum::Json(json!({"model":"prx-decision-model","answers":answers}))
}

#[tokio::test]
async fn actual_compiler_binds_decision_model_before_real_http() {
    let workspace = tempfile::tempdir().expect("owned workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("verified local main model"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let dispatches = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(vec![
        tool("file_read", &[], &dispatches),
        tool("image_info", &[], &dispatches),
    ]);
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let memory: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("owned actual SQLite"));
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
                allowed_tools: None,
                direct_names: BTreeSet::from(["file_read".to_owned()]),
                ..SurfacePolicy::default()
            },
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .expect("source-owned actual builtin service");
    assert_eq!(
        service.factory.request_model(),
        None,
        "builtin preserves the main-model sealing fallback"
    );
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        "inspect file_read project evidence",
        "prx_model_binding_run",
    )
    .await;
    let context = service
        .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
        .await
        .expect("actual admitted builtin hop");
    drain_required(&service).await;
    let hop = context.audit_hops().next().expect("actual first hop");
    assert!(hop.request_compiled && hop.scored_count > 0);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("owned loopback");
    let endpoint = format!("http://{}", listener.local_addr().expect("actual address"));
    let capture = Capture(Arc::new(parking_lot::Mutex::new(Vec::new())));
    let router = axum::Router::new()
        .route("/v1/systemone", axum::routing::post(answer))
        .with_state(capture.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("owned server");
    });
    let mut decision_config = config.clone();
    decision_config.causal_tree.backend.kind =
        crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
    decision_config.causal_tree.backend.endpoint = endpoint.clone();
    decision_config.causal_tree.backend.model = "prx-decision-model".to_owned();
    decision_config.causal_tree.backend.timeout_ms = 1000;
    decision_config
        .causal_tree
        .validate()
        .expect("legal configured component");
    decision_config.providers.models.push(serde_json::from_value(json!({"provider":format!("custom:{endpoint}"),"model":"prx-decision-model","operations":["decision_endpoint"]})).expect("PMM decision declaration"));
    let catalog = ProviderCatalogView::from_config(&decision_config);
    let factory = DecisionBackendFactory::from_config(
        &decision_config,
        FactoryDeps {
            catalog: &catalog,
            key: Some(Arc::clone(&service.state_key)),
            transports: &crate::decision::compatible_http::HttpDecisionTransports,
            config_generation: generation.id.0,
            events: Arc::new(SilentEvents),
        },
    )
    .expect("actual configured HTTP factory");
    let compiled = QuestionCompiler::compile(
        &hop.state,
        &hop.candidates,
        &CompilerPolicy {
            key: &service.state_key,
            model: factory.request_model(),
            expires_at: Instant::now() + Duration::from_secs(5),
            limits: RequestLimits::default(),
        },
    )
    .expect("real compiler over real admitted candidates");
    let request = compiled.request();
    let result = factory
        .decide(request, &CausalLineage::default())
        .await
        .expect("actual configured response");
    let repeated = factory
        .decide(request, &CausalLineage::default())
        .await
        .expect("coherent cache hit");
    server.abort();
    assert!(server.await.expect_err("owned server cancelled").is_cancelled());
    assert_eq!(request.model(), Some("prx-decision-model"));
    assert_eq!(
        hop.state.trusted.parts().resolved_model.as_ref(),
        "ollama:prx-local-model"
    );
    assert_eq!(result.served_by, ServedBy::Primary);
    assert_eq!(repeated.served_by, ServedBy::Cache);
    assert_eq!(result.served.identity().configured_model.as_ref(), "prx-decision-model");
    let bodies = capture.0.lock();
    assert_eq!(bodies.len(), 1, "a valid cached repeat performs no I/O");
    assert_eq!(
        bodies.first().expect("actual body").get("model"),
        Some(&json!("prx-decision-model"))
    );
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
}
