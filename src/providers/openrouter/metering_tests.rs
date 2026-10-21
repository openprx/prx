//! Loopback proofs for refused and streaming OpenRouter turns.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn refused_unpriced_turn_logs_once_and_never_calls_the_provider() -> anyhow::Result<()> {
    use crate::agent::{agent::Agent, dispatcher::XmlToolDispatcher};
    use crate::providers::credential_probe::CredentialProbe;
    use tracing::instrument::WithSubscriber;

    let workspace = tempfile::tempdir()?;
    let log_path = workspace.path().join("warnings.log");
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::fs::File::create(&log_path)?)
        .finish();
    let probe = CredentialProbe::start().await;
    let mut provider = OpenRouterProvider::new(Some("prx-test-credential"));
    provider.chat_endpoint = Some(format!("{}/chat/completions", probe.base_url()));
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.default_provider = Some("openrouter".into());
    config.default_model = Some("deepseek/deepseek-v3.2".into());
    config.api_key = Some("prx-test-credential".into());
    config.auth.codex_auth_json_auto_import = false;
    let mut agent = Agent::builder()
        .runtime_config(config, "openrouter")
        .provider(Box::new(provider))
        .tools(Vec::new())
        .memory(Arc::new(crate::memory::SqliteMemory::new(workspace.path())?))
        .observer(Arc::new(crate::observability::NoopObserver {}))
        .tool_dispatcher(Box::new(XmlToolDispatcher))
        .build()?;
    async {
        for turn in 1..=2 {
            let reply = agent.turn("What is 2 + 2?").await?;
            assert_eq!(reply, "I can't complete this request under the current policy.");
            let log = std::fs::read_to_string(&log_path)?;
            assert_eq!(
                log.matches("G6 budget admission refuses unpriced model").count(),
                turn,
                "{log}"
            );
            assert!(log.contains("openrouter/deepseek/deepseek-v3.2"));
            assert!(log.contains(r#"[cost.prices."deepseek/deepseek-v3.2"]"#));
            assert!(probe.requests().is_empty());
            assert!(!workspace.path().join("state/costs.jsonl").exists());
        }
        anyhow::Ok(())
    }
    .with_subscriber(subscriber)
    .await
}

struct StreamServer(tokio::task::JoinHandle<()>);
impl Drop for StreamServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn priced_streamed_turn_requests_usage_and_writes_cost_ledger() -> anyhow::Result<()> {
    use crate::agent::terminal::{TurnDeliveryIntent, TurnTerminalDraft, TurnTerminalStatus, TurnTerminalTelemetry};
    use crate::agent::turn_contracts::ChatMode;
    use crate::agent::turn_request::{TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
    use crate::memory::{Memory, MemoryFabric, SqliteMemory};
    use axum::{
        Json, Router,
        http::{StatusCode, header},
        routing::post,
    };

    let requests = Arc::new(AtomicUsize::new(0));
    let received = Arc::clone(&requests);
    let app = Router::new().route("/chat/completions", post(move |Json(body): Json<serde_json::Value>| {
        let received = Arc::clone(&received);
        async move {
            received.fetch_add(1, Ordering::Relaxed);
            if body.get("stream").and_then(serde_json::Value::as_bool) != Some(true)
                || body.pointer("/usage/include").and_then(serde_json::Value::as_bool) != Some(true) {
                return (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "text/plain")], "stream must request usage");
            }
            (StatusCode::OK, [(header::CONTENT_TYPE, "text/event-stream")], concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"PRX streamed reply\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"total_tokens\":120,\"prompt_tokens_details\":{\"cached_tokens\":30,\"cache_write_tokens\":10}}}\n\n",
                "data: [DONE]\n\n"
            ))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/chat/completions", listener.local_addr()?);
    let _server = StreamServer(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let workspace = tempfile::tempdir()?;
    let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    owner
        .scope(Box::pin(async {
            let mut provider = OpenRouterProvider::new(Some("prx-test-credential"));
            provider.chat_endpoint = Some(endpoint);
            let model = "anthropic/claude-sonnet-5";
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_owned();
            config.config_path = workspace.path().join("config.toml");
            config.default_provider = Some("openrouter".into());
            config.default_model = Some(model.into());
            config.api_key = Some("prx-test-credential".into());
            config.auth.codex_auth_json_auto_import = false;
            config.cost.prices.insert(
                model.into(),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 2.0,
                    cache_read: 0.1,
                    cache_write: 2.0,
                },
            );
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path())?);
            let registry = Arc::new(Vec::new());
            let input = "What is 2 + 2?";
            let admission = crate::agent::test_admission::AdmittedTurn::with_enabled_builtin_model(
                config.clone(),
                input,
                Arc::clone(&registry),
                Arc::clone(&memory),
                ("openrouter", model),
            )
            .await;
            let mut adapter = admission.adapter();
            adapter.stream_provider = true;
            let envelope = adapter.tool_execution_context.envelope.clone();
            let decision = admission.decision();
            let guard = crate::runtime::turn_terminal::TurnTerminalGuard::new(
                owner.register(tokio_util::sync::CancellationToken::new())?,
                Arc::clone(decision.service),
                Arc::clone(decision.context),
                MemoryFabric::new(Arc::clone(&memory), workspace.path().to_string_lossy()),
                Arc::new(config),
                envelope.clone(),
                None,
            )?;
            let started_at = chrono::Utc::now();
            let mut history = vec![ChatMessage::user(input)];
            let (reply, trace) = crate::agent::turn_engine::run_turn(
                &mut history,
                TurnRequest {
                    channel_name: "cli",
                    scope: None,
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &crate::config::MultimodalConfig::default(),
                    schedule: TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: Some(admission.compaction()),
                    decision: admission.decision(),
                },
                TurnServices {
                    provider: &provider,
                    provider_name: "openrouter",
                    model,
                    temperature: 0.0,
                    tools_registry: registry,
                    observer: &crate::observability::NoopObserver,
                    hooks: &crate::hooks::HookManager::new(workspace.path().to_owned()),
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(adapter),
                    memory: admission.memory_runtime(),
                },
                TurnDelivery {
                    cancellation: None,
                    on_delta: None,
                    on_tool_call: None,
                },
            )
            .await?;
            assert_eq!(reply.into_text(), "PRX streamed reply");
            assert_eq!(requests.load(Ordering::Relaxed), 1);
            assert_eq!(
                trace.tokens_used,
                TokenUsage::reported_with_cache(Some(100), Some(20), Some(120), Some(10), Some(30))
            );
            let route = crate::llm::route_decision::RouteDecision::single_candidate("openrouter", model);
            let outcome = crate::agent::terminal::provider_outcome_from_trace(&route, started_at, trace);
            guard
                .commit(TurnTerminalDraft {
                    decision: None,
                    terminal_id: decision.proof.run_id().to_owned(),
                    scope: envelope.message_scope(),
                    status: TurnTerminalStatus::Completed,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(outcome),
                    telemetry: TurnTerminalTelemetry {
                        summary: "PRX streamed reply".into(),
                        started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: TurnDeliveryIntent::ReturnToCaller,
                })
                .await?;
            let ledger = std::fs::read_to_string(workspace.path().join("state/costs.jsonl"))?;
            let rows: Vec<crate::cost::types::CostRecord> =
                ledger.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
            assert_eq!(rows.len(), 1);
            let row = rows.first().unwrap();
            assert_eq!(row.settlement_provider.as_deref(), Some("openrouter"));
            assert_eq!(row.usage.model, "openrouter/anthropic/claude-sonnet-5");
            assert_eq!(row.usage.input_tokens, 100);
            assert_eq!(row.usage.output_tokens, 20);
            assert_eq!(row.cache_read_input_tokens, Some(30));
            assert_eq!(row.cache_creation_input_tokens, Some(10));
            assert!((row.usage.cost_usd - 0.000_123).abs() < 1e-12, "{}", row.usage.cost_usd);
            assert!(row.settlement_id.is_some());
            decision
                .service
                .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(5))
                .await;
            anyhow::Ok(())
        }))
        .await
}
