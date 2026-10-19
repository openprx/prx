#![cfg(test)]
//! Genuine Agent source -> selected/persisted history read -> paid embedding.
use super::*;
use axum::{Json, Router, extract::State, routing::post};
use futures::FutureExt;
use std::{panic::AssertUnwindSafe, time::Duration};
use tokio_util::sync::CancellationToken;

const EMBED_MODEL: &str = "prx-router-embedding";

async fn build(workspace: &std::path::Path, endpoint: &str) -> (Agent, Arc<ObservedSqlite>, Arc<AtomicUsize>) {
    let observed = Arc::new(ObservedSqlite::new(SqliteMemory::new(workspace).unwrap()));
    let memory: Arc<dyn Memory> = observed.clone();
    let policy = RouterConfig {
        alpha: 1.0,
        knn_k: 1,
        knn_min_records: 1,
        models: vec![RouterModelConfig {
            model_id: MODEL.into(),
            provider: "ollama".into(),
            cost_per_million_tokens: 0.0,
            max_context: 128_000,
            reserved_output_tokens: None,
            latency_ms: 1,
            categories: vec!["conversation".into()],
            elo_rating: 1_000.0,
        }],
        ..RouterConfig::default()
    };
    let mut config = Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config.config_path = workspace.join("config.toml");
    config.default_model = Some(MODEL.into());
    config.causal_tree.enabled = true;
    config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
    config.router = policy.clone();
    // The real Router Retrieve hop is subject to G7, including PMM's
    // NativeTools requirement for this exact controlled model.
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"ollama\"\nmodel = \"{MODEL}\"\noperations = [\"chat\"]\nnative_tools = true\n"
        ))
        .unwrap(),
    );
    let resolved = crate::providers::selection::check(
        &config,
        "ollama",
        MODEL,
        &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )
    .expect("controlled Router model must satisfy the real PMM tool requirement");
    assert_eq!(
        resolved.verification(),
        &crate::providers::catalog::Verification::Verified
    );
    config.memory.embedding_provider = endpoint.to_owned();
    config.memory.embedding_model = EMBED_MODEL.to_owned();
    config.memory.embedding_dimensions = 384;
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider": endpoint, "model": EMBED_MODEL, "operations": ["embedding"]
        }))
        .unwrap(),
    );
    config.cost.prices.insert(
        format!("{endpoint}/{EMBED_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 0.001,
            output: 0.001,
            cache_write: 0.001,
            cache_read: 0.001,
        },
    );
    assert!(
        matches!(
            crate::llm::route_decision::usage_cost_resolution(endpoint, EMBED_MODEL, 1, 0, 0, 0, &config.cost),
            crate::llm::route_decision::UsagePrice::Priced(cost) if cost > 0.0
        ),
        "the actual embedding identity must have a positive declared price"
    );
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    let embedder = crate::memory::create_embedder_from_config(&config);
    assert!(embedder.supports_attempt_accounting());
    let router = RouterEngine::new(
        policy,
        "ollama".into(),
        Vec::new(),
        Arc::clone(&memory),
        Some(embedder),
        &crate::config::schema::CostConfig::default(),
    )
    .await
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::builder()
        .runtime_config(config, "ollama")
        .provider(Box::new(LocalProvider(Arc::clone(&calls))))
        .tools(vec![Box::new(crate::tools::memory_search::MemorySearchTool::new(
            workspace.to_owned(),
            Arc::clone(&memory),
            true,
        ))])
        .memory(memory)
        .observer(Arc::new(crate::observability::NoopObserver {}))
        .tool_dispatcher(Box::new(XmlToolDispatcher))
        .router(router)
        .build()
        .unwrap();
    agent.seed_scoped_query_memory_for_test(INPUT).await;
    (agent, observed, calls)
}

#[derive(Clone)]
struct ServerState {
    workspace: std::path::PathBuf,
    requests: Arc<AtomicUsize>,
    known_usage: bool,
    observed: Arc<parking_lot::Mutex<Option<Arc<ObservedSqlite>>>>,
}
async fn embedding(State(state): State<ServerState>, Json(body): Json<Value>) -> Json<Value> {
    let observed = state
        .observed
        .lock()
        .clone()
        .expect("actual memory owner published before turn");
    assert_eq!(
        observed.observations.lock().read_calls,
        1,
        "actual selected history read must complete before physical embedding"
    );
    // Check the real SQLite causal decision exists before the paid physical send.
    let database = Connection::open_with_flags(
        state.workspace.join("self/causal_tree/decisions.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let count: i64 = database
        .query_row(
            "SELECT count(*) FROM decisions WHERE source_event_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(count > 0, "physical embedding requires an already persisted decision");
    assert_eq!(body.get("model").and_then(Value::as_str), Some(EMBED_MODEL));
    assert!(
        body.get("input")
            .and_then(Value::as_array)
            .is_some_and(|inputs| inputs.len() == 1)
    );
    state.requests.fetch_add(1, Ordering::SeqCst);
    let mut response = serde_json::json!({"data":[{"index":0,"embedding":vec![0.25_f32;384]}]});
    if state.known_usage {
        response.as_object_mut().expect("embedding response object").insert(
            "usage".to_owned(),
            serde_json::json!({"prompt_tokens":10,"total_tokens":10}),
        );
    }
    Json(response)
}

async fn run(known_usage: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("custom:http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let observed_slot = Arc::new(parking_lot::Mutex::new(None));
    let app = Router::new()
        .route("/v1/embeddings", post(embedding))
        .with_state(ServerState {
            workspace: workspace.path().to_owned(),
            requests: Arc::clone(&requests),
            known_usage,
            observed: Arc::clone(&observed_slot),
        });
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
    });
    let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let result = AssertUnwindSafe(Box::pin(async {
        let (mut agent, observed, calls) = build(workspace.path(), &endpoint).await;
        *observed_slot.lock() = Some(Arc::clone(&observed));
        let turn = cleanup.scope(Box::pin(agent.turn(INPUT)));
        let reply = tokio::time::timeout(Duration::from_secs(5), turn).await.unwrap();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "training must reuse the acquired vector, not send again; retained workspace={}",
            workspace.path().display()
        );
        let rows: Vec<Value> = std::fs::read_to_string(workspace.path().join("state/costs.reservations.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let dispatches: Vec<_> = rows
            .iter()
            .filter(|row| row.get("event").and_then(Value::as_str) == Some("dispatched"))
            .collect();
        assert_eq!(dispatches.len(), 1);
        let attempt = dispatches
            .first()
            .unwrap()
            .get("attempt_id")
            .and_then(Value::as_str)
            .unwrap();
        assert!(attempt.starts_with("router_embedding:"));
        assert_eq!(
            rows.iter()
                .filter(|row| row.get("event").and_then(Value::as_str) == Some("estimated_settled_v1"))
                .count(),
            usize::from(known_usage)
        );
        let db = Connection::open(workspace.path().join("memory/brain.db")).unwrap();
        let terminals: i64 = db
            .query_row(
                "SELECT count(*) FROM message_events WHERE event_type='turn.finalized'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(terminals, 1);
        if known_usage {
            assert_eq!(reply.unwrap(), "The expression adds two integers.");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let (count, bytes): (i64, i64) = db
                .query_row("SELECT count(*),length(embedding) FROM router_history_v1", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap();
            assert_eq!((count, bytes), (1, 384 * 4));
            let observation = observed.observations.lock();
            assert_eq!(observation.read_calls, 1);
            assert_eq!(observation.commit_calls, 1);
            assert_eq!(observation.last_read.as_ref().unwrap().as_ref().unwrap().count, 0);
            assert_eq!(observation.last_commit.as_ref().unwrap(), &Ok(()));
            let raw: String = db
                .query_row(
                    "SELECT raw_payload_json FROM message_events WHERE event_type='turn.finalized'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let terminal: Value = serde_json::from_str(&raw).unwrap();
            let receipt = terminal
                .get("terminal_replay_v1")
                .and_then(|replay| replay.get("router_read"))
                .expect("persisted Router read projection");
            assert_eq!(
                receipt.get("readmission_succeeded").and_then(Value::as_bool),
                Some(true)
            );
            let read_id = receipt.get("read_decision_id").and_then(Value::as_str).unwrap();
            let next_id = receipt.get("next_decision_id").and_then(Value::as_str).unwrap();
            assert_ne!(read_id, next_id);
            let decisions = Connection::open(workspace.path().join("self/causal_tree/decisions.db")).unwrap();
            let prior: String = decisions
                .query_row(
                    "SELECT prior_decision_id FROM decisions WHERE decision_id=?1",
                    [next_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                prior, read_id,
                "paid read must be part of the actual two-hop decision chain"
            );
        } else {
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "unknown embedding usage holds tokens instead of funding main inference"
            );
            assert!(
                !rows
                    .iter()
                    .any(|row| row.get("event").and_then(Value::as_str) == Some("released"))
            );
        }
    }))
    .catch_unwind()
    .await;
    let drained = cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(2))
        .await;
    stop.cancel();
    let joined = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
    let server_confirmed = matches!(joined, Ok(Ok(Ok(()))));
    if !server_confirmed {
        server.abort();
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    }
    if drained.is_err() || !server_confirmed {
        let retained = workspace.keep();
        panic!(
            "Router fixture cleanup unconfirmed; evidence retained at {}",
            retained.display()
        );
    }
    if let Err(panic) = result {
        // Retain the real journal and terminal after confirmed owned cleanup.
        // The original assertion already identifies this exact workspace.
        let _retained = workspace.keep();
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_paid_router_read_accounts_once_and_training_reuses_vector() {
    Box::pin(run(true)).await;
}

#[tokio::test]
async fn actual_paid_router_unknown_usage_keeps_hold_before_main_inference() {
    Box::pin(run(false)).await;
}
