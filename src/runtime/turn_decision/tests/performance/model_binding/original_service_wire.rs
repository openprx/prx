//! Actual service request bytes plus original-object observation in an owned fixture.
//! Not a BASE comparison or release latency/allocation measurement.
use super::*;
use futures::FutureExt;

#[derive(Clone)]
struct WireCapture {
    raw: Arc<parking_lot::Mutex<Vec<(Vec<u8>, Vec<u8>)>>>,
    delay: Duration,
}

async fn receive_original(
    axum::extract::State(capture): axum::extract::State<WireCapture>,
    raw: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    assert!(raw.len() <= 1024 * 1024);
    // Retain incoming octets BEFORE JSON parsing. No expected-body reconstruction.
    let index = {
        let mut records = capture.raw.lock();
        assert!(records.len() < 8, "bounded actual physical attempts");
        let index = records.len();
        records.push((raw.to_vec(), Vec::new()));
        index
    };
    let body: Value = serde_json::from_slice(&raw).expect("actual controlled wire JSON");
    let response = super::answer(
        axum::extract::State(super::Capture(Arc::new(parking_lot::Mutex::new(Vec::new())))),
        axum::Json(body),
    )
    .await
    .0;
    let response = serde_json::to_vec(&response).expect("actual fixture response bytes");
    capture.raw.lock().get_mut(index).expect("captured actual request").1 = response.clone();
    tokio::time::sleep(capture.delay).await;
    ([(axum::http::header::CONTENT_TYPE, "application/json")], response).into_response()
}

#[tokio::test]
#[ignore = "explicit current-service original wire observation, outside performance windows"]
async fn actual_configured_service_original_wire_ok() {
    witness(false).await;
}

#[tokio::test]
#[ignore = "explicit current-service slow transport observation, outside performance windows"]
async fn actual_configured_service_original_wire_slow() {
    witness(true).await;
}

// Keep the test-runtime block_on frame independent of the owned fixture state.
// These boxes are fixture overhead, never release allocation acceptance.
#[inline(never)]
fn witness(slow: bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> {
    Box::pin(run_witness(slow))
}

async fn run_witness(slow: bool) {
    let output =
        std::path::PathBuf::from(std::env::var("PRX_ORIGINAL_WIRE_OUTPUT").expect("explicit owned evidence directory"));
    assert!(output.is_absolute() && output.is_dir());
    let output = output.join(if slow { "slow" } else { "ok" });
    std::fs::create_dir(&output).expect("fresh leg; never overwrite evidence");
    let diagnostic_timing = crate::runtime::decision_accounting::timing::Capture::new();
    let workspace = output.join("workspace");
    std::fs::create_dir(&workspace).expect("fresh retained SQLite workspace");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("owned literal loopback");
    let endpoint = format!("http://{}", listener.local_addr().expect("owned endpoint"));
    let capture = WireCapture {
        raw: Arc::new(parking_lot::Mutex::new(Vec::new())),
        delay: Duration::from_millis(if slow { 200 } else { 0 }),
    };
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let router = axum::Router::new()
        .route("/v1/systemone", axum::routing::post(receive_original))
        .with_state(capture.clone());
    let mut server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
    });
    let cleanup =
        crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new()).expect("actual cleanup owner");
    let mut business_panic = None;
    let mut business_error = None;
    let owned_work: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + '_>> = Box::pin(async {
        // Retain the service outside the unwind boundary so both durable queues
        // are acknowledged even when a business assertion or returned error fails.
        let mut retained_service = None;
        let business = std::panic::AssertUnwindSafe(async {
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.clone();
        config.config_path = workspace.join("config.toml");
        let provider = format!("custom:{endpoint}");
        for declaration in [
            json!({"provider":"ollama","model":"prx-local-model","operations":["chat"],"native_tools":true}),
            json!({"provider":provider,"model":"prx-decision-model","operations":["decision_endpoint"]}),
        ] { config.providers.models.push(serde_json::from_value(declaration).expect("actual PMM operation declaration")); }
        for key in ["ollama/prx-local-model".to_owned(), format!("{provider}/prx-decision-model")] {
            config.cost.prices.insert(key, crate::config::schema::ModelPricing { input:1.0, output:1.0, cache_write:1.0, cache_read:1.0 });
        }
        assert!(matches!(crate::llm::route_decision::usage_cost_resolution(&provider,"prx-decision-model",1,0,0,0,&config.cost),crate::llm::route_decision::UsagePrice::Priced(cost) if cost>0.0));
        config = crate::config::manage::mutation::causal::CausalConfigService::mutate(
            &config,
            &[
                crate::config::manage::mutation::causal::CausalMutation::SetEnabled(true),
                crate::config::manage::mutation::causal::CausalMutation::SetPolicy(
                    crate::config::manage::mutation::causal::PolicyPatch {
                        extra_latency_budget_ms: Some(500),
                        ..Default::default()
                    },
                ),
                crate::config::manage::mutation::causal::CausalMutation::SetBackend {
                    kind: crate::decision::BackendKind::CompatibleHttp,
                    endpoint: Some(endpoint.clone()),
                    model: Some("prx-decision-model".to_owned()),
                    pinned_snapshot: None,
                    timeout_ms: Some(100),
                },
            ],
        )
        .expect("actual fixture decision configuration through its owner");
        config.causal_tree.validate().expect("unchanged 100/500ms legal policy");
        let generation = crate::config::new_shared(config.clone()).pin();
        let registry = Arc::new(Vec::<Box<dyn Tool>>::new());
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(Arc::clone(&registry),Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),Arc::new(DenyApprovalStrategy),Arc::new(AdapterOwnedPreparation),Arc::new(TracingToolExecutionAudit)));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&workspace).expect("actual SQLite"));
        let service = build_turn_decision_service(TurnSurfaceKind::AgentLoop,TurnServiceBuild { parent_grant:None,generation:Arc::clone(&generation),registry,execution,catalog_sources:sources(&generation,&config),surface_policy:SurfacePolicy::default(),provider_catalog:&provider_catalog,transports:&crate::decision::compatible_http::HttpDecisionTransports,events:Arc::new(SilentEvents),memory:Arc::clone(&memory),security }).expect("actual configured service");
        retained_service = Some(service);
        let service = retained_service.as_ref().expect("retained actual service");
        assert_eq!(service.factory.request_model(),Some("prx-decision-model"));
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(&generation,Arc::clone(&memory),"Hello PRX, reply with a brief greeting.","prx_original_wire_run").await;
        let cancellation = CancellationToken::new();
        let started=Instant::now();
        let (context, observed)=diagnostic_timing.scope(Box::pin(crate::causal_tree::decision_engine::original_observation::capture("actual-compatible-service-wire",service.decide_agent_initial(&proof,AgentTurnKind::Repl,&cancellation)))).await.expect("actual original observer");
        let elapsed=started.elapsed();
        let context=context.expect("actual configured decision or bounded builtin fallback");
        let hops=context.audit_hops().collect::<Vec<_>>();
        let sidecar=observed.checked_sidecar(&hops).expect("actual original objects match returned hop inventory");
        assert!(observed.checked_sidecar(&[]).is_err());
        std::fs::write(output.join("original-sidecar.json"),serde_json::to_vec_pretty(&sidecar)? )?;
        assert!(service.drain_writes(Instant::now()+Duration::from_secs(5)).await,"both actual durable queues acknowledged before readback");
        let mut primary=0;
        for hop in &hops {
            let parts=hop.state.trusted.parts();
            assert_eq!(parts.owner_id.as_deref(),Some(proof.owner_id()));
            assert_eq!(parts.source_event_id.as_deref(),proof.source_event_id());
            let workspace_id=service.store.workspace_id().expect("actual durable workspace");
            let row=service.store.reader().get(&DecisionScope::Owner {workspace_id,owner_id:Arc::from(proof.owner_id())},&hop.decision_id).expect("actual scoped durable read").expect("actual durable hop");
            assert_eq!(row.run_id.as_deref(),Some(proof.run_id()));
            assert_eq!(row.session_key.as_deref(),Some(proof.session_key()));
            if let Some(prediction)=&hop.prediction {
                if prediction.served_by==ServedBy::Primary { primary+=1; }
            }
        }
        let bodies=capture.raw.lock();
        assert!(!bodies.is_empty(),"configured service must physically reach owned HTTP; builtin alone cannot pass");
        for (index,(request,response)) in bodies.iter().enumerate() {
            std::fs::write(output.join(format!("request-{index}.bin")),request)?;
            std::fs::write(output.join(format!("constructed-response-{index}.bin")),response)?;
            let parsed:Value=serde_json::from_slice(request)?;
            assert_eq!(parsed.get("model"),Some(&json!("prx-decision-model")));
            let keys=parsed.as_object().expect("actual wire object").keys().map(String::as_str).collect::<BTreeSet<_>>();
            assert_eq!(keys,BTreeSet::from(["model","questions","state"]));
        }
        if slow { assert_eq!(primary,0,"200ms controlled response must not pass the 100ms backend deadline"); } else { assert!(primary>0,"ok configured response must actually serve a hop"); }
        assert!(elapsed<=Duration::from_millis(500),"original observation exceeded unchanged turn budget: {elapsed:?}");
        Ok::<(), anyhow::Error>(())
        }).catch_unwind().await;
        match business {
            Ok(Ok(())) => {}
            Ok(Err(error)) => business_error = Some(error),
            Err(panic) => business_panic = Some(panic),
        }
        let drained = match retained_service.as_ref() {
            Some(service) => service.drain_writes(Instant::now() + Duration::from_secs(5)).await,
            None => true, // No service was constructed, hence no service queue exists.
        };
        std::fs::write(
            output.join("durable-drain.json"),
            serde_json::to_vec_pretty(&json!({
                "service_constructed":retained_service.is_some(),"both_queues_drained":drained,
                "business_panicked":business_panic.is_some(),"business_error":business_error.as_ref().map(|error|format!("{error:#}")),
            }))?,
        )?;
        anyhow::ensure!(drained, "actual service durable queues did not drain");
        // Drop service inside work; CleanupScope.run drains its tickets next.
        Ok(())
    });
    let work = cleanup.run(owned_work).await;
    stop.cancel();
    let joined = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
    if joined.is_err() {
        server.abort();
        let _ = server.await;
        panic!("owned HTTP shutdown exceeded deadline; aborted and joined");
    }
    joined
        .expect("checked owned HTTP timeout")
        .expect("owned HTTP task join")
        .expect("owned HTTP serve result");
    // Preserve physical bytes even when service admission/observer/assertions fail.
    for (index, (request, response)) in capture.raw.lock().iter().enumerate() {
        std::fs::write(output.join(format!("request-{index}.bin")), request).expect("retain original received bytes");
        std::fs::write(output.join(format!("constructed-response-{index}.bin")), response)
            .expect("retain constructed fixture reply, not delivery proof");
    }
    std::fs::write(output.join("cleanup-result.json"),serde_json::to_vec_pretty(&json!({
        "cleanup_completed":work.is_ok(),"cleanup_error":work.as_ref().err().map(|error|format!("{error:#}")),
        "business_panicked":business_panic.is_some(),"business_error":business_error.as_ref().map(|error|format!("{error:#}")),
    })).expect("cleanup result JSON")).expect("retain cleanup result");
    diagnostic_timing
        .write(&output.join("accounting-timing.json"))
        .expect("retain numeric actual-stage timings");
    work.expect("whole source-owned cleanup completed before propagating business failure");
    if let Some(panic) = business_panic {
        std::panic::resume_unwind(panic);
    }
    if let Some(error) = business_error {
        panic!("actual service witness failed: {error:#}");
    }
}
