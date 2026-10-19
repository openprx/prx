#![cfg(test)]

//! Actual auxiliary transport and existing durable receipt, without ingress claims.
use super::*;
use crate::agent::terminal::{TurnDeliveryIntent, TurnTerminalDraft, TurnTerminalStatus, TurnTerminalTelemetry};
use crate::memory::{Memory, MemoryFabric, MemoryVisibility, MessageEventScope, SqliteMemory};
use crate::providers::{ChatMessage, ChatRequest, Provider, catalog::ModelParameter, parameter_capture::Capture};
use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn every_auxiliary_purpose_retains_prepared_parameters_after_success_or_failure() -> anyhow::Result<()> {
    for purpose in [
        AuxiliaryPurpose::SignalVision,
        AuxiliaryPurpose::SmartGroupClassifier,
        #[cfg(feature = "llm-router")]
        AuxiliaryPurpose::AutomixPremium,
    ] {
        for fails in [false, true] {
            let workspace = tempfile::tempdir()?;
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let sender = Arc::new(parking_lot::Mutex::new(Some(sender)));
            let app = Router::new().route(
                "/v1/chat/completions",
                post(move |Json(body): Json<Value>| {
                    let sender = Arc::clone(&sender);
                    async move {
                        let pending = sender.lock().take();
                        if let Some(sender) = pending {
                            let _ = sender.send(body);
                        }
                        let status = if fails {
                            StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            StatusCode::OK
                        };
                        (
                            status,
                            Json(json!({
                                "choices":[{"message":{"role":"assistant","content":"prx reply"}}],
                                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                            })),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let base = format!("http://{}/v1", listener.local_addr()?);
            let _server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_owned();
            config.default_provider = Some("openai".into());
            config.default_model = Some("gpt-4o".into());
            config.api_url = Some(base.clone());
            config.api_key = Some("prx-test-credential".into());
            let config = Arc::new(config);
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path())?);
            let fabric = MemoryFabric::new(memory, workspace.path().to_string_lossy())
                .with_event_recording(config.memory.event_recording_config());
            let selected = crate::providers::selection::check_with(
                &config,
                &crate::providers::selection::catalog_for(&config),
                "openai",
                "gpt-4o",
                crate::providers::credentials::CredentialRef::LegacyInline(
                    crate::providers::credentials::LegacySlot::Root,
                ),
                &crate::providers::catalog::Requirement::chat(),
                crate::providers::catalog::UnknownPolicy::Invocation,
            )?;
            let provider: Arc<dyn Provider> = Arc::new(crate::providers::openai::OpenAiProvider::with_base_url(
                Some(&base),
                Some("prx-test-credential"),
            ));
            let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
            let key = crate::decision::DecisionKey::ephemeral();
            let accounting_scope = Arc::new(crate::runtime::decision_accounting::AccountingScope::new(
                Arc::clone(&config),
                Arc::clone(&key),
                Arc::clone(&cleanup),
                [1; 32],
                crate::causal_tree::state::TurnBudgets {
                    tokens: 8000,
                    micro_usd: 1_000_000,
                    latency_ms: 500,
                },
                Instant::now() + Duration::from_millis(500),
            ));
            let accounting = AuxiliaryAccounting::new(
                selected,
                ("openai".into(), "gpt-4o".into()),
                Arc::clone(&provider),
                key,
                accounting_scope,
                purpose,
                Default::default(),
            )?;
            let route = RouteDecision::single_candidate_for_context(
                "openai",
                "gpt-4o",
                "prx_owner",
                "prx_session",
                None,
                None,
                purpose.label(),
                8,
                false,
                false,
            );
            let observation = AuxiliaryProviderCall::new_accounted(purpose, route, &accounting);
            observation.begin()?;
            let outer = Capture::new(true);
            let messages = [ChatMessage::user("prx parameter request")];
            let result = outer
                .scope(crate::providers::attempt_accounting::chat_traced(
                    provider.as_ref(),
                    ChatRequest {
                        messages: &messages,
                        tools: None,
                    },
                    "gpt-4o",
                    3.0,
                    accounting.clone(),
                ))
                .await;
            cleanup.drain((Instant::now() + Duration::from_secs(5)).into()).await?;
            let body = tokio::time::timeout(Duration::from_secs(5), receiver).await??;
            assert_eq!(result.is_err(), fails);
            if let Ok(trace) = &result {
                observation.complete(trace)?;
            }
            assert!(
                outer.take().is_empty(),
                "auxiliary recording cannot borrow the outer turn recorder"
            );
            let prepared = accounting.prepared_parameters();
            assert_eq!(prepared.len(), 1);
            assert_eq!(prepared.first().unwrap().model, "openai/gpt-4o");
            assert_eq!(
                prepared.first().unwrap().catalog_revision,
                crate::providers::catalog::ReleaseCatalog::embedded_ref()?.revision
            );
            assert_eq!(body.get("model"), Some(&json!("gpt-4o")));
            assert_eq!(body.get("temperature"), Some(&json!(2.0)));
            assert_eq!(
                prepared.first().unwrap().values.get(&ModelParameter::Temperature),
                body.get("temperature")
            );
            let now = chrono::Utc::now();
            let mut draft = TurnTerminalDraft {
                decision: None,
                terminal_id: "prx_aux_terminal".into(),
                scope: MessageEventScope::new("test", MemoryVisibility::Workspace)
                    .with_owner_id("prx_owner")
                    .with_session_key("prx_session"),
                status: if fails {
                    TurnTerminalStatus::Failed
                } else {
                    TurnTerminalStatus::Completed
                },
                history: None,
                history_scope: None,
                provider_outcome: None,
                telemetry: TurnTerminalTelemetry {
                    summary: "prx auxiliary parameters".into(),
                    started_at: now,
                    finished_at: now,
                },
                delivery_intent: TurnDeliveryIntent::None,
            };
            let event = observation.persist(&fabric, &config, &mut draft).await?.unwrap();
            let payload = event.raw_payload_json.as_deref().unwrap();
            let decoded: Value = serde_json::from_str(payload)?;
            assert_eq!(decoded.get("usage_uncertain"), Some(&json!(fails)));
            assert_eq!(
                decoded.get("prepared_parameters"),
                Some(&serde_json::to_value(&prepared)?)
            );
            assert_eq!(event.owner_id.as_deref(), Some("prx_owner"));
            assert_eq!(decoded.get("purpose").and_then(Value::as_str), Some(purpose.label()));
            assert!(!payload.contains("prx parameter request"));
            assert!(!payload.contains("prx-test-credential"));
        }
    }
    Ok(())
}
