#![cfg(test)]

//! Real admitted transport proof for release parameter provenance.
use crate::providers::catalog::ModelParameter;
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn admitted_turn_retains_the_actual_serialized_parameters() {
    use crate::agent::turn_contracts::ChatMode;
    use crate::agent::turn_request::{TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
    use crate::memory::{Memory, SqliteMemory};
    use crate::providers::ChatMessage;
    use axum::{Json, Router, routing::post};
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(sender)));
    let app = Router::new().route("/api/chat", post(move |Json(body): Json<serde_json::Value>| {
        let sender = Arc::clone(&sender);
        async move {
            let pending_sender = sender.lock().take();
            if let Some(sender) = pending_sender {
                let _ = sender.send(body);
            }
            Json(json!({"message":{"role":"assistant","content":"prx reply"},"done":true,"prompt_eval_count":5,"eval_count":2}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let workspace = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).unwrap());
    let registry = crate::agent::turn_tests::admission::registry_with_search(Vec::new());
    let input = "Reply with a short greeting.";
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_enabled_builtin_model(
        config,
        input,
        Arc::clone(&registry),
        Arc::clone(&memory),
        ("ollama", "llama3.2"),
    )
    .await;
    let provider = crate::providers::ollama::OllamaProvider::new(Some(&url), None);
    let mut history = vec![ChatMessage::user(input.to_owned())];
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
            compaction: None,
            decision: admission.decision(),
        },
        TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "llama3.2",
            temperature: 3.0,
            tools_registry: registry,
            observer: &crate::observability::NoopObserver,
            hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: admission.memory_runtime(),
        },
        TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reply.into_text(), "prx reply");
    let body = receiver.await.unwrap();
    assert_eq!(body.pointer("/options/temperature").unwrap().clone(), 2.0);
    assert_eq!(trace.prepared_parameters.len(), 1);
    assert_eq!(
        admission.decision().context.prepared_main_parameters(),
        trace.prepared_parameters,
        "the actual engine request must also be retained by its issued terminal owner"
    );
    assert_eq!(trace.prepared_parameters.first().unwrap().model, "ollama/llama3.2");
    assert_eq!(
        trace
            .prepared_parameters
            .first()
            .unwrap()
            .values
            .get(&ModelParameter::Temperature)
            .unwrap()
            .clone(),
        body.pointer("/options/temperature").unwrap().clone()
    );
    let decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
        "ollama",
        "llama3.2",
        "prx_owner",
        "prx-session",
        None,
        None,
        "prx-test",
        64,
        false,
        false,
    );
    let outcome = crate::agent::terminal::provider_outcome_from_trace(&decision, chrono::Utc::now(), trace);
    assert_eq!(outcome.prepared_parameters.len(), 1);
    assert!(!serde_json::to_string(&outcome).unwrap().contains(input));
    drop(admission);
    server.abort();
}

#[tokio::test]
async fn prepared_parameters_survive_scope_drop_without_fabricating_a_provider_outcome() -> anyhow::Result<()> {
    use crate::agent::terminal::{TurnDeliveryIntent, TurnTerminalDraft, TurnTerminalStatus, TurnTerminalTelemetry};
    use crate::memory::{Memory, SqliteMemory};
    use crate::providers::parameter_wire::{self, Format};
    use crate::runtime::turn_cleanup::CleanupTracker;
    use std::time::Duration;

    for status in [TurnTerminalStatus::Cancelled, TurnTerminalStatus::Failed] {
        let workspace = tempfile::tempdir()?;
        let cleanup = Arc::new(CleanupTracker::default());
        let result = cleanup.scope(async {
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_owned();
            let cost = config.cost.clone();
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path())?);
            let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_enabled_builtin_model(
                config, "Reply with a short greeting.",
                crate::agent::turn_tests::admission::registry_with_search(Vec::new()),
                Arc::clone(&memory), ("ollama", "llama3.2"),
            ).await;
            let capture = admission.decision().context.capture_main_parameters(true);
            let body = json!({"model":"llama3.2","messages":[{"role":"user","content":"prx prepared request"}],"options":{"temperature":3.0}});
            let mut encoded = None;
            {
                let mut work = Box::pin(capture.scope(async {
                    encoded = Some(serde_json::to_value(parameter_wire::request("ollama", Format::Ollama, "llama3.2", &body)).unwrap());
                    std::future::pending::<()>().await;
                }));
                anyhow::ensure!(futures::poll!(work.as_mut()).is_pending(), "preparation must reach the suspended provider scope");
                // Drop the actual task-local future, as cancellation does.
                // No transport or successful provider response is fabricated.
            }
            drop(capture);
            let prepared = admission.decision().context.prepared_main_parameters();
            let runtime = admission.memory_runtime();
            let fabric = runtime.event_fabric.unwrap();
            let scope = runtime.request_event_scope.unwrap();
            let terminal_id = scope.run_id.clone().unwrap();
            let decision = admission.decision();
            let settlement = decision.service.terminal_settlement_ref(Arc::clone(decision.context))?;
            let now = chrono::Utc::now();
            let draft = TurnTerminalDraft {
                decision: Some(settlement), terminal_id, scope, status,
                history: None, history_scope: None, provider_outcome: None,
                telemetry: TurnTerminalTelemetry { summary: "prx prepared call interrupted".into(), started_at: now, finished_at: now },
                delivery_intent: TurnDeliveryIntent::None,
            };
            let receipt = crate::agent::terminal::finalize_turn(&fabric, draft, &cost, workspace.path()).await?;
            let rejoined = decision
                .service
                .rejoin_terminal(&receipt.terminal_id, receipt.terminal_event.owner_id.as_deref().unwrap())
                .await?;
            let events = memory.list_message_events_since(&admission.principal(), 0, 100).await?;
            Ok::<_, anyhow::Error>((prepared, encoded.unwrap(), receipt, events, rejoined))
        }).await;
        let drained = cleanup
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await;
        let (prepared, body, receipt, events, rejoined) = result?;
        drained?;
        assert_eq!(rejoined, crate::causal_tree::store::Enqueue::Queued);
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared.first().unwrap().model, "ollama/llama3.2");
        assert_eq!(body.pointer("/options/temperature"), Some(&json!(2.0)));
        assert_eq!(
            prepared.first().unwrap().values.get(&ModelParameter::Temperature),
            body.pointer("/options/temperature")
        );
        let payload: serde_json::Value =
            serde_json::from_str(receipt.terminal_event.raw_payload_json.as_deref().unwrap())?;
        assert_eq!(
            payload.get("prepared_parameters"),
            Some(&serde_json::to_value(prepared)?)
        );
        let release = crate::providers::catalog::ReleaseCatalog::embedded_ref()?;
        assert_eq!(
            payload
                .get("model_catalog_revision")
                .and_then(serde_json::Value::as_str),
            Some(release.revision.as_str())
        );
        assert_eq!(
            payload
                .get("model_catalog_content_sha256")
                .and_then(serde_json::Value::as_str),
            Some(release.content_sha256())
        );
        assert_eq!(payload.get("status"), Some(&serde_json::to_value(status)?));
        assert!(payload.get("provider_outcome").unwrap().is_null());
        assert!(receipt.assistant_event.is_none());
        assert!(receipt.usage_settlement.is_none());
        assert!(receipt.cost_settlement.is_none());
        assert!(
            !events
                .iter()
                .any(|event| event.event_type.starts_with("provider.") || event.event_type == "usage.settled")
        );
        assert!(!payload.to_string().contains("prx prepared request"));
    }
    Ok(())
}
