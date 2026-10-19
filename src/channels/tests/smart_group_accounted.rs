#![cfg(test)]
//! Real smart-group admission and auxiliary classifier accounting.
use super::*;
use axum::{Json, Router, extract::State, routing::post};
use futures_util::FutureExt as _;
use serde_json::{Value, json};
use std::time::Duration;

const PROVIDER: &str = "openai";
const MAIN_MODEL: &str = "prx-smart-main";
const CLASSIFIER_MODEL: &str = "prx-smart-classifier";

const MAIN_REPLY: &str = "prx smart main reply";

/// What the loopback classifier endpoint answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    No,
    Yes,
    ClassifierFails,
}

#[derive(Clone)]
struct WireState {
    requests: Arc<parking_lot::Mutex<Vec<Value>>>,
    verdict: Verdict,
}

async fn respond(State(state): State<WireState>, Json(body): Json<Value>) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let classifier = body.get("model").and_then(Value::as_str) == Some(CLASSIFIER_MODEL);
    state.requests.lock().push(body);
    let (model, content) = if classifier {
        match state.verdict {
            Verdict::No => (CLASSIFIER_MODEL, "NO"),
            Verdict::Yes => (CLASSIFIER_MODEL, "YES"),
            Verdict::ClassifierFails => {
                return (axum::http::StatusCode::BAD_REQUEST, "classifier refused").into_response();
            }
        }
    } else {
        (MAIN_MODEL, MAIN_REPLY)
    };
    Json(json!({
        "id": "prx-smart-response",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 9, "completion_tokens": 1, "total_tokens": 10}
    }))
    .into_response()
}

struct WireOwner {
    task: Option<tokio::task::JoinHandle<()>>,
    stop: CancellationToken,
}

impl Drop for WireOwner {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl WireOwner {
    async fn close(mut self) -> Result<(), String> {
        self.stop.cancel();
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
            Ok(result) => result.map_err(|error| format!("loopback server join failed: {error}")),
            Err(_) => {
                task.abort();
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut task).await;
                Err("owned smart-group loopback listener did not drain".to_owned())
            }
        }
    }
}

fn channel_turn(
    ctx: Arc<ChannelRuntimeContext>,
    message: traits::ChannelMessage,
    cancellation: CancellationToken,
) -> std::pin::Pin<Box<impl std::future::Future<Output = ()>>> {
    Box::pin(run_owned_channel_probe(ctx, message, cancellation))
}

/// A classifier YES, or a classifier failure that fails open, runs the full
/// agent loop. The turn must complete once and persist the main reply plus the
/// classifier's physical accounting beside the main call's.
async fn assert_classifier_and_main_receipts(
    verdict: Verdict,
    content: &str,
    requests: &Arc<parking_lot::Mutex<Vec<Value>>>,
    workspace: &std::path::Path,
    memory: &dyn Memory,
    channel_impl: &RecordingChannel,
) {
    let models: Vec<String> = requests
        .lock()
        .iter()
        .filter_map(|request| request.get("model").and_then(Value::as_str).map(str::to_owned))
        .collect();
    assert_eq!(
        models.first().map(String::as_str),
        Some(CLASSIFIER_MODEL),
        "the classifier runs first: {models:?}"
    );
    assert!(
        models.iter().any(|model| model == MAIN_MODEL),
        "the full agent loop must reach the main model: {models:?}"
    );
    assert!(
        format!("{:?}", response_messages(&channel_impl.sent_messages.lock().await)).contains(MAIN_REPLY),
        "the main reply must be delivered"
    );

    let principal = crate::runtime::envelope::RuntimeEnvelope::channel_with_session(
        workspace.to_string_lossy(),
        "test-channel_prx_user",
        "test-channel",
        "prx_user",
        "prx_group",
        crate::memory::MemoryVisibility::Session,
    )
    .memory_principal();
    let events = memory
        .list_message_events_since(&principal, 0, 200)
        .await
        .expect("smart-group runtime events");
    assert!(
        events
            .iter()
            .any(|event| event.role == "user" && event.content == content)
    );
    assert!(
        events
            .iter()
            .any(|event| event.role == "assistant" && event.content.contains(MAIN_REPLY)),
        "the main reply is durable history"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn.finalized")
            .count(),
        1,
        "the turn finalizes exactly once"
    );
    let settled: Vec<Value> = events
        .iter()
        .filter(|event| event.event_type == "provider.auxiliary.settled")
        .filter_map(|event| event.raw_payload_json.as_deref())
        .filter_map(|payload| serde_json::from_str(payload).ok())
        .collect();
    let classifier_receipts = settled
        .iter()
        .filter(|payload| payload.get("purpose").and_then(Value::as_str) == Some("smart_group_classifier"))
        .count();
    assert_eq!(
        classifier_receipts, 1,
        "the classifier receipt persists on the Completed turn"
    );
    for main_receipt in ["usage.settled", "provider.final_outcome"] {
        assert!(
            events.iter().any(|event| event.event_type == main_receipt),
            "the main call's {main_receipt} receipt persists beside the classifier's"
        );
    }

    let rows: Vec<Value> = std::fs::read_to_string(workspace.join("state/costs.reservations.jsonl"))
        .expect("physical accounting journal")
        .lines()
        .map(|line| serde_json::from_str(line).expect("accounting row"))
        .collect();
    let dispatched: Vec<&str> = rows
        .iter()
        .filter(|row| row["event"] == "dispatched")
        .filter_map(|row| row["attempt_id"].as_str())
        .collect();
    assert_eq!(
        dispatched.len(),
        1,
        "the classifier reserves exactly once: {dispatched:?}"
    );
    assert!(dispatched[0].starts_with("smart_group_classifier:"));
    let settled_in_ledger = rows
        .iter()
        .any(|row| row["event"] == "estimated_settled_v1" && row["attempt_id"] == dispatched[0]);
    assert_eq!(
        settled_in_ledger,
        verdict == Verdict::Yes,
        "a reported YES usage settles its reservation; a failed classifier call reports none"
    );
}

async fn exercise_accounted_classifier(content: &str, verdict: Verdict) {
    let workspace = tempfile::tempdir().expect("smart-group workspace");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("owned smart-group listener");
    let endpoint = format!("http://{}/v1", listener.local_addr().expect("loopback address"));
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let stop = CancellationToken::new();
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(WireState {
            requests: Arc::clone(&requests),
            verdict,
        });
    let shutdown = stop.clone();
    let server = WireOwner {
        stop,
        task: Some(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .expect("owned smart-group server");
        })),
    };
    let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let outcome = std::panic::AssertUnwindSafe(async {
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        config.default_provider = Some(PROVIDER.to_owned());
        config.default_model = Some(MAIN_MODEL.to_owned());
        config.api_url = Some(endpoint.clone());
        config.api_key = Some("prx-loopback-fixture-key".to_owned());
        config.smart_group.classifier_provider = PROVIDER.to_owned();
        config.smart_group.classifier_model = CLASSIFIER_MODEL.to_owned();
        for (model, native_tools) in [(MAIN_MODEL, true), (CLASSIFIER_MODEL, false)] {
            config.providers.models.push(
                serde_json::from_value(json!({
                    "provider": PROVIDER,
                    "model": model,
                    "operations": ["chat"],
                    "native_tools": native_tools
                }))
                .expect("smart-group model declaration"),
            );
            config.cost.prices.insert(
                format!("{PROVIDER}/{model}"),
                crate::config::schema::ModelPricing {
                    input: 0.001,
                    output: 0.001,
                    cache_write: 0.001,
                    cache_read: 0.001,
                },
            );
        }
        let resolved = crate::providers::selection::check(
            &config,
            PROVIDER,
            MAIN_MODEL,
            &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )
        .expect("actual selected smart-group main model");
        let prepared_provider: Arc<dyn Provider> = Arc::from(
            crate::providers::create_provider_for(
                &resolved,
                &crate::providers::selection::catalog_for(&config),
                &config.reliability,
                &config.cost,
                &crate::providers::provider_runtime_options_from_config(&config),
            )
            .expect("actual smart-group main provider factory"),
        );
        let channel_impl = Arc::new(RecordingChannel::default());
        let mut ctx = gate_test_ctx(
            crate::security::AutonomyLevel::Supervised,
            Arc::new(CountingMemory::default()),
            channel_impl.clone(),
        );
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("smart-group sqlite"));
        let shared = crate::config::new_shared(config);
        let context = Arc::get_mut(&mut ctx).expect("unique smart-group context");
        context.config = Arc::clone(&shared);
        context.config_generation = shared.pin();
        context.default_provider = Arc::new(PROVIDER.to_owned());
        context.provider = prepared_provider;
        context.model = Arc::new(MAIN_MODEL.to_owned());
        context.api_url = Some(endpoint);
        context.api_key = Some("prx-loopback-fixture-key".to_owned());
        context.memory = Arc::clone(&memory);
        context.tools_registry = Arc::new(vec![
            Box::new(crate::tools::stay_silent::StaySilentTool) as Box<dyn Tool>
        ]);
        context.workspace_dir = Arc::new(workspace.path().to_owned());
        context.hooks = Arc::new(HookManager::new(workspace.path().to_owned()));
        context.channels_by_name = Arc::new(HashMap::from([(
            "test-channel".to_owned(),
            channel_impl.clone() as Arc<dyn Channel>,
        )]));
        context
            .group_reply_mode_by_channel
            .insert("test-channel".to_owned(), crate::config::GroupReplyMode::Smart);
        context.security = Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::from_config(
                &shared.pin().effective.autonomy,
                workspace.path(),
            )),
        }));

        let mut message = gate_test_message();
        message.id = "prx-smart-group-accounted".to_owned();
        message.sender = "prx_user".to_owned();
        message.reply_target = "prx_group".to_owned();
        message.content = content.to_owned();
        message.chat_kind = crate::channels::traits::ChatKind::Group;
        message.is_group_hint = true;

        let turn = cleanup.scope(channel_turn(ctx.clone(), message, CancellationToken::new()));
        tokio::time::timeout(Duration::from_secs(5), turn)
            .await
            .expect("admitted smart-group turn must finish");

        if verdict != Verdict::No {
            assert_classifier_and_main_receipts(
                verdict,
                content,
                &requests,
                workspace.path(),
                memory.as_ref(),
                &channel_impl,
            )
            .await;
            return;
        }
        {
            let captured = requests.lock();
            assert_eq!(captured.len(), 1, "classifier NO must avoid the full agent loop");
            assert_eq!(
                captured[0].get("model").and_then(Value::as_str),
                Some(CLASSIFIER_MODEL),
                "the pinned resolver-selected classifier model must be used on wire"
            );
            assert!(
                captured[0]
                    .get("messages")
                    .and_then(Value::as_array)
                    .is_some_and(|messages| messages.iter().any(|message| {
                        message.get("role").and_then(Value::as_str) == Some("system")
                            && message
                                .get("content")
                                .and_then(Value::as_str)
                                .is_some_and(|content| content.contains("relevance gate"))
                    })),
                "the one physical request must be the smart-group classifier"
            );
        }
        assert!(
            ctx.provider_cache.lock().is_empty(),
            "the decision-owned classifier must not use the channel provider cache"
        );
        assert!(
            response_messages(&channel_impl.sent_messages.lock().await).is_empty(),
            "an admitted classifier NO must emit no channel response"
        );

        let principal = crate::runtime::envelope::RuntimeEnvelope::channel_with_session(
            workspace.path().to_string_lossy(),
            "test-channel_prx_user",
            "test-channel",
            "prx_user",
            "prx_group",
            crate::memory::MemoryVisibility::Session,
        )
        .memory_principal();
        let events = memory
            .list_message_events_since(&principal, 0, 100)
            .await
            .expect("smart-group runtime events");
        let input = events
            .iter()
            .find(|event| event.role == "user" && event.content == content)
            .expect("durable smart-group input");
        let auxiliary = events
            .iter()
            .find(|event| event.event_type == "provider.auxiliary.settled")
            .expect("durable classifier auxiliary settlement");
        let payload: Value = serde_json::from_str(
            auxiliary
                .raw_payload_json
                .as_deref()
                .expect("classifier settlement payload"),
        )
        .expect("classifier settlement JSON");
        assert_eq!(
            payload.get("purpose").and_then(Value::as_str),
            Some("smart_group_classifier")
        );
        assert_eq!(
            payload.pointer("/provider_outcome/final_model").and_then(Value::as_str),
            Some(CLASSIFIER_MODEL)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1,
            "the silent turn must finalize exactly once"
        );
        assert!(
            events.iter().all(|event| event.role != "assistant"),
            "classifier silence must not invent assistant history"
        );

        let connection = rusqlite::Connection::open_with_flags(
            workspace.path().join("self/causal_tree/decisions.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("actual smart-group decision store");
        let admitted: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM decisions WHERE source_event_id=?1",
                [input.event_id.as_str()],
                |row| row.get(0),
            )
            .expect("source-bound smart-group decisions");
        assert!(admitted > 0, "the source decision must exist before classifier silence");

        let rows: Vec<Value> = std::fs::read_to_string(workspace.path().join("state/costs.reservations.jsonl"))
            .expect("classifier physical accounting journal")
            .lines()
            .map(|line| serde_json::from_str(line).expect("accounting row"))
            .collect();
        let dispatched: Vec<_> = rows.iter().filter(|row| row["event"] == "dispatched").collect();
        assert_eq!(dispatched.len(), 1, "one classifier request must reserve once");
        let attempt_id = dispatched[0]["attempt_id"].as_str().expect("classifier attempt id");
        assert!(attempt_id.starts_with("smart_group_classifier:"));
        assert!(
            rows.iter()
                .any(|row| { row["event"] == "estimated_settled_v1" && row["attempt_id"] == attempt_id }),
            "reported classifier usage must settle its reservation"
        );
    })
    .catch_unwind()
    .await;

    let drained = cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(2))
        .await;
    let listener_closed = server.close().await;
    if drained.is_err() || listener_closed.is_err() || outcome.is_err() {
        let retained = workspace.keep();
        println!("PRX_SMART_GROUP_FAILURE_EVIDENCE={}", retained.display());
        assert!(
            drained.is_ok() && listener_closed.is_ok(),
            "smart-group cleanup unconfirmed: cleanup={drained:?}, listener={listener_closed:?}"
        );
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn smart_group_classifier_runs_after_decision_admission_with_typed_accounting() {
    exercise_accounted_classifier("the deployment completed an hour ago across the project", Verdict::No).await;
}

#[tokio::test]
async fn smart_group_heuristic_noise_requires_accounted_classifier_before_silence() {
    exercise_accounted_classifier("lol", Verdict::No).await;
}

#[test]
fn a_skip_without_a_settled_classifier_outcome_fails_open_into_the_loop() {
    let now = chrono::Utc::now();
    let settled = crate::llm::route_decision::ProviderExecutionOutcome {
        prepared_parameters: Vec::new(),
        decision_id: "prx_smart_fixture".to_owned(),
        started_at: now,
        finished_at: now,
        attempts: Vec::new(),
        final_provider: PROVIDER.to_owned(),
        final_model: CLASSIFIER_MODEL.to_owned(),
        status: crate::llm::route_decision::ExecutionStatus::Success,
        fallback_reason: None,
        tokens_used: Default::default(),
    };
    let gate = |outcome, provider_outcome| AdmittedSmartPreGate {
        outcome,
        provider_outcome,
        started_at: now,
    };
    let skip = pre_gate::PreGateOutcome::skip(pre_gate::PreGatePath::ClassifierSkip);
    let enter = pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierEnter);
    assert!(
        gate(skip, None).silence_provider_outcome().is_none(),
        "a skip with no settled receipt must not silence the message"
    );
    assert_eq!(
        gate(skip, Some(settled.clone())).silence_provider_outcome(),
        Some(settled.clone())
    );
    assert!(
        gate(enter, Some(settled)).silence_provider_outcome().is_none(),
        "an enter outcome never silences"
    );
}

#[tokio::test]
async fn smart_group_classifier_yes_runs_the_full_loop_and_persists_both_receipts() {
    exercise_accounted_classifier("the deployment completed an hour ago across the project", Verdict::Yes).await;
}

#[tokio::test]
async fn smart_group_classifier_failure_fails_open_and_persists_both_receipts() {
    exercise_accounted_classifier(
        "the deployment completed an hour ago across the project",
        Verdict::ClassifierFails,
    )
    .await;
}
