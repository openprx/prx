//! Turn-local, content-free capture at the actual request serializer boundary.

use super::catalog::parameters::ResolvedParameters;
use parking_lot::Mutex;
use std::{future::Future, sync::Arc};

type Records = Arc<Mutex<Vec<ResolvedParameters>>>;
tokio::task_local! { static CURRENT: Option<Records>; }

#[derive(Clone)]
pub(crate) struct Capture(Option<Records>);

impl Capture {
    pub(crate) fn new(enabled: bool) -> Self {
        Self(enabled.then(|| Arc::new(Mutex::new(Vec::new()))))
    }

    pub(crate) fn scope<F: Future>(&self, work: F) -> impl Future<Output = F::Output> + use<F> {
        // The returned future owns its recorder. It must not inherit the
        // lifetime of this short-lived handle or an accounting-owner borrow.
        CURRENT.scope(self.0.clone(), work)
    }

    #[cfg(test)]
    pub(crate) fn take(&self) -> Vec<ResolvedParameters> {
        self.0
            .as_ref()
            .map_or_else(Vec::new, |records| std::mem::take(&mut *records.lock()))
    }

    pub(crate) fn snapshot(&self) -> Vec<ResolvedParameters> {
        self.snapshot_from(0)
    }

    pub(crate) fn snapshot_from(&self, offset: usize) -> Vec<ResolvedParameters> {
        self.0.as_ref().map_or_else(Vec::new, |records| {
            records.lock().iter().skip(offset).cloned().collect()
        })
    }
}

/// Retain the active serializer recorder when layering accounting around an
/// already admitted main solver call. Absence remains disabled recording.
pub(crate) fn current() -> Capture {
    Capture(CURRENT.try_with(Clone::clone).unwrap_or_default())
}

pub(crate) fn record(parameters: ResolvedParameters) {
    // No recorder means event recording is disabled for this turn. Parameter
    // resolution still runs, but it must not create a separate persistence path.
    let _ = CURRENT.try_with(|records| {
        if let Some(records) = records {
            records.lock().push(parameters);
        }
    });
}

pub(crate) fn spawn<F>(work: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // No active scope is the same as explicitly disabled recording. A child
    // inherits that absence as well, rather than borrowing a sibling's scope.
    let records = CURRENT.try_with(Clone::clone).unwrap_or_default();
    let settings = super::model_settings::inherited();
    tokio::spawn(super::model_settings::scope(settings, CURRENT.scope(records, work)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        catalog::ModelParameter,
        parameter_wire::{self, Format},
    };
    use serde_json::json;

    fn prepare(model: &str) {
        let body = json!({"temperature":0.3,"messages":[{"role":"user","content":"prx private request"}]});
        let request = parameter_wire::request("kimi-code", Format::Compatible, model, &body);
        assert_eq!(
            serde_json::to_value(request)
                .unwrap()
                .get("temperature")
                .unwrap()
                .clone(),
            1.0
        );
    }

    #[tokio::test]
    async fn a_scoped_future_owns_its_recorder_and_remains_send_after_the_handle_drops() {
        let records = Capture::new(true);
        let future = {
            let handle = records.clone();
            handle.scope(async {
                prepare("k3");
                17
            })
        };
        assert_eq!(tokio::spawn(future).await.unwrap(), 17);
        let prepared = records.take();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared.first().unwrap().model, "kimi-code/k3");
    }

    #[tokio::test]
    async fn custom_anthropic_transport_keeps_its_own_parameter_identity() {
        use axum::{Json, Router, routing::post};
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let app = Router::new().route("/v1/messages", post(move |Json(body): Json<serde_json::Value>| {
            let sender = Arc::clone(&sender);
            async move {
                let pending_sender = sender.lock().take();
                if let Some(sender) = pending_sender {
                    let _ = sender.send(body);
                }
                Json(json!({"content":[{"type":"text","text":"prx reply"}],"usage":{"input_tokens":1,"output_tokens":1}}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let identity = format!("anthropic-custom:http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = crate::providers::create_provider(&identity, Some("prx-test-credential")).unwrap();
        let capture = Capture::new(true);
        let reply = capture
            .scope(provider.chat_with_system(None, "prx request", "claude-sonnet-5", 0.3))
            .await
            .unwrap();
        let body = receiver.await.unwrap();
        server.abort();
        assert_eq!(reply, "prx reply");
        assert_eq!(body.get("temperature"), Some(&json!(0.3)));
        assert!(body.get("output_config").is_none());
        assert!(body.get("thinking").is_none());
        assert!(
            capture.take().is_empty(),
            "a custom identity has no original-provider release evidence"
        );
    }

    #[tokio::test]
    async fn model_failover_resolves_each_actual_request_independently() {
        use crate::providers::{
            ChatMessage, ChatRequest, Provider, openai::OpenAiProvider, reliable::ReliableProvider,
        };
        use axum::{Json, Router, http::StatusCode, routing::post};
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<serde_json::Value>| {
                let seen = Arc::clone(&seen);
                async move {
                    let fails = body.get("model").and_then(serde_json::Value::as_str) == Some("gpt-5.6-sol");
                    seen.lock().push(body);
                    if fails {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"error":{"message":"prx fixture unavailable"}})),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({"choices":[{"message":{"role":"assistant","content":"prx reply"}}]})),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = ReliableProvider::new(
            vec![(
                "openai".into(),
                Box::new(OpenAiProvider::with_base_url(Some(&base), Some("prx-test-credential"))),
            )],
            0,
            1,
        )
        .with_model_fallbacks([("gpt-5.6-sol".into(), vec!["gpt-4o".into()])].into());
        let capture = Capture::new(true);
        let messages = [ChatMessage::user("prx request")];
        let trace = capture
            .scope(provider.chat_traced(
                ChatRequest {
                    messages: &messages,
                    tools: None,
                },
                "gpt-5.6-sol",
                0.3,
            ))
            .await
            .unwrap();
        server.abort();
        assert_eq!(trace.final_model, "gpt-4o");
        let bodies = bodies.lock();
        assert_eq!(bodies.len(), 2);
        let first = bodies.first().unwrap();
        let second = bodies.get(1).unwrap();
        assert_eq!(first.get("model"), Some(&json!("gpt-5.6-sol")));
        assert!(first.get("temperature").is_none());
        assert_eq!(first.get("reasoning_effort"), Some(&json!("medium")));
        assert_eq!(second.get("model"), Some(&json!("gpt-4o")));
        assert_eq!(second.get("temperature"), Some(&json!(0.3)));
        assert!(second.get("reasoning_effort").is_none());
        assert!(second.get("verbosity").is_none());
        let records = capture.take();
        assert_eq!(records.len(), 2);
        assert_eq!(records.first().unwrap().model, "openai/gpt-5.6-sol");
        assert_eq!(records.get(1).unwrap().model, "openai/gpt-4o");
        assert!(
            !records
                .get(1)
                .unwrap()
                .values
                .contains_key(&ModelParameter::ReasoningEffort)
        );
        assert_eq!(
            records.get(1).unwrap().values.get(&ModelParameter::Temperature),
            second.get("temperature")
        );
    }

    #[tokio::test]
    async fn concurrent_turns_and_their_streaming_tasks_keep_separate_parameter_records() {
        async fn run(model: &'static str) -> Vec<ResolvedParameters> {
            let capture = Capture::new(true);
            capture
                .scope(async move {
                    prepare(model);
                    spawn(async move {
                        prepare(model);
                    })
                    .await
                    .unwrap();
                })
                .await;
            capture.take()
        }
        let (left, right) = tokio::join!(run("k3"), run("kimi-for-coding"));
        assert_eq!(left.len(), 2);
        assert_eq!(right.len(), 2);
        for record in left {
            assert_eq!(record.model, "kimi-code/k3");
            assert_eq!(record.values.get(&ModelParameter::Temperature).unwrap().clone(), 1.0);
        }
        for record in right {
            assert_eq!(record.model, "kimi-code/kimi-for-coding");
        }
    }

    #[tokio::test]
    async fn disabled_child_recording_does_not_borrow_the_parent_recorder() {
        let parent = Capture::new(true);
        let child = Capture::new(false);
        parent
            .scope(async {
                prepare("k3");
                child
                    .scope(async {
                        spawn(async {
                            prepare("kimi-for-coding");
                        })
                        .await
                        .unwrap();
                    })
                    .await;
                prepare("k3");
            })
            .await;
        assert_eq!(child.take(), Vec::new());
        let records = parent.take();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record.model == "kimi-code/k3"));
    }

    #[tokio::test]
    async fn prepared_parameters_persist_in_the_existing_owner_bound_outcome_event() {
        use crate::llm::route_decision::{ProviderExecutionOutcome, RouteDecision, record_provider_outcome_events};
        use crate::memory::{Memory, MemoryFabric, MemoryPrincipal, MemoryVisibility, MessageEventScope, SqliteMemory};
        let capture = Capture::new(true);
        capture
            .scope(async {
                prepare("k3");
            })
            .await;
        let decision = RouteDecision::single_candidate_for_context(
            "kimi-code",
            "k3",
            "prx_owner",
            "prx-session",
            None,
            None,
            "prx-test",
            64,
            false,
            false,
        );
        let outcome = ProviderExecutionOutcome::success_for_decision(&decision, chrono::Utc::now())
            .with_prepared_parameters(capture.take());
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(Arc::clone(&memory), "prx_workspace");
        let scope = MessageEventScope::new("runtime", MemoryVisibility::Workspace)
            .with_owner_id("prx_owner")
            .with_session_key("prx-session");
        record_provider_outcome_events(&fabric, scope, &outcome).await.unwrap();
        let principal = MemoryPrincipal {
            require_owner_match: true,
            workspace_id: "prx_workspace".into(),
            owner_id: Some("prx_owner".into()),
            session_key: Some("prx-session".into()),
            agent_id: None,
            persona_id: None,
            channel: None,
            sender: None,
            legacy_session_key: None,
        };
        let events = memory.list_message_events_since(&principal, 0, 10).await.unwrap();
        assert_eq!(events.len(), 2, "parameters add no separate event write");
        let event = events
            .iter()
            .find(|event| event.event_type == "provider.final_outcome")
            .unwrap();
        assert_eq!(event.owner_id.as_deref(), Some("prx_owner"));
        let payload = event.raw_payload_json.as_deref().unwrap();
        let decoded: ProviderExecutionOutcome = serde_json::from_str(payload).unwrap();
        assert_eq!(decoded.prepared_parameters.len(), 1);
        assert_eq!(decoded.prepared_parameters.first().unwrap().model, "kimi-code/k3");
        assert_eq!(
            decoded
                .prepared_parameters
                .first()
                .unwrap()
                .values
                .get(&ModelParameter::Temperature)
                .unwrap()
                .clone(),
            1.0
        );
        assert_eq!(
            decoded.prepared_parameters.first().unwrap().catalog_revision,
            "2026-10-31.3"
        );
        assert!(!payload.contains("prx private request"));
        assert!(!payload.contains("messages"));
        let legacy = ProviderExecutionOutcome::success_for_decision(&decision, chrono::Utc::now());
        let legacy_json = serde_json::to_string(&legacy).unwrap();
        assert!(!legacy_json.contains("prepared_parameters"));
        assert_eq!(
            serde_json::from_str::<ProviderExecutionOutcome>(&legacy_json)
                .unwrap()
                .prepared_parameters,
            Vec::new()
        );
    }
}
