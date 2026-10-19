#![cfg(test)]
use super::*;

/// Physical provider transport for the two retained redaction probes only.
async fn run_physical_channel_probe(tmp: &TempDir, memory: Arc<dyn Memory>, channel: Arc<dyn Channel>, fail: bool) {
    use axum::response::IntoResponse;
    use futures_util::FutureExt;
    #[cfg(unix)]
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("owned channel provider listener");
    let endpoint = format!("http://{}", listener.local_addr().expect("owned provider address"));
    let requests = Arc::new(AtomicUsize::new(0));
    let invalid = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&requests);
    let bad = Arc::clone(&invalid);
    let handler = axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
        let calls = Arc::clone(&calls);
        let bad = Arc::clone(&bad);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            if body.get("model").and_then(serde_json::Value::as_str) != Some("prx-channel-probe")
                || body.get("stream").and_then(serde_json::Value::as_bool) == Some(true)
            {
                bad.fetch_add(1, Ordering::SeqCst);
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            }
            let secret = crate::agent::terminal::persistence_probe::PROBE_SECRET;
            if fail {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"error": {"message": format!("upstream rejected key {secret}")}})),
                )
                    .into_response()
            } else {
                axum::Json(serde_json::json!({
                    "choices": [{"index": 0, "message": {"role": "assistant",
                        "content": format!("Here is the key {secret} you asked for")}, "finish_reason": "stop"}]
                }))
                .into_response()
            }
        }
    });
    let app = axum::Router::new()
        .route("/chat/completions", handler.clone())
        .route("/v1/chat/completions", handler)
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024));
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let mut server = ChannelProbeServer {
        stop,
        task: Some(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        })),
    };
    // Catch setup, decision-driver and owner failures before joining this listener.
    let outcome = std::panic::AssertUnwindSafe(async {
        let evidence = tmp.path().join("decision-evidence");
        let mut directory = std::fs::DirBuilder::new();
        #[cfg(unix)]
        directory.mode(0o700);
        directory
            .create(&evidence)
            .expect("fresh private channel decision evidence");
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .expect("owned channel decision listener")
            .retain_evidence_at(evidence);
        let decision_endpoint = driver.endpoint().to_owned();
        driver
            .run_factory(true, || async {
                let provider_name = format!("custom:{endpoint}");
                let mut config = Config::default();
                config.workspace_dir = tmp.path().to_owned();
                config.config_path = tmp.path().join("config.toml");
                config.autonomy.level = crate::security::AutonomyLevel::Supervised;
                config.default_provider = Some(provider_name.clone());
                config.default_model = Some("prx-channel-probe".to_owned());
                // Compatible transport implements native schema serialization; the
                // registry remains empty and this probe requests Answer only.
                config.providers.models.push(
                    serde_json::from_value(serde_json::json!({
                        "provider": provider_name, "model": "prx-channel-probe",
                        "operations": ["chat"], "native_tools": true
                    }))
                    .expect("physical channel model declaration"),
                );
                config.cost.prices.insert(
                    format!("{provider_name}/prx-channel-probe"),
                    crate::config::schema::ModelPricing {
                        input: 1.0,
                        output: 1.0,
                        cache_write: 1.0,
                        cache_read: 1.0,
                    },
                );
                crate::agent::agent::native_decision_fixture::configure(&mut config, &decision_endpoint)
                    .expect("physical channel decision endpoint");
                let resolved =
                    providers::selection::check_default(&config, providers::catalog::UnknownPolicy::Invocation)
                        .expect("real channel PMM selection");
                let catalog = providers::selection::catalog_for(&config);
                let provider: Arc<dyn Provider> = Arc::from(
                    providers::create_provider_for(
                        &resolved,
                        &catalog,
                        &config.reliability,
                        &config.cost,
                        &providers::provider_runtime_options_from_config(&config),
                    )
                    .expect("real channel provider factory"),
                );
                assert!(
                    provider
                        .capabilities_for(
                            "prx-channel-probe",
                            crate::providers::traits::ProviderRequestMode::NonStreaming,
                        )
                        .native_tool_calling
                );
                let security = crate::security::SecurityPolicy::from_config(&config.autonomy, tmp.path());
                let mut ctx = channel_turn_probe_ctx(tmp, memory, provider, channel);
                let owned = Arc::get_mut(&mut ctx).expect("unshared physical channel context");
                owned.default_provider = Arc::new(provider_name);
                owned.model = Arc::new("prx-channel-probe".to_owned());
                owned.config = crate::config::new_shared(config);
                owned.config_generation = owned.config.pin();
                owned.security = Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                    security: Arc::new(security),
                }));
                run_owned_channel_probe(ctx, gate_test_message(), CancellationToken::new()).await;
            })
            .await;
    })
    .catch_unwind()
    .await;
    server.stop.cancel();
    let task = server.task.as_mut().expect("owned provider listener handle");
    let joined = tokio::time::timeout(Duration::from_secs(5), &mut *task).await;
    if joined.is_err() {
        task.abort();
        let _ = (&mut *task).await;
    }
    // Keep the handle in the RAII owner through every suspension above.
    // Both normal completion and timeout abort have now been joined.
    drop(server.task.take());
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    joined
        .expect("provider listener stopped")
        .expect("provider listener joined")
        .expect("provider listener served");
    assert!(
        requests.load(Ordering::SeqCst) > 0,
        "real channel provider must receive a request"
    );
    assert_eq!(
        invalid.load(Ordering::SeqCst),
        0,
        "provider requests must match the configured physical model"
    );
}

/// Retain the owned runtime database through cleanup and after a failed oracle.
struct RetainedChannelProbeWorkspace(Option<TempDir>);

impl RetainedChannelProbeWorkspace {
    fn new() -> Self {
        Self(Some(TempDir::new().expect("test: owned channel workspace")))
    }
}

impl std::ops::Deref for RetainedChannelProbeWorkspace {
    type Target = TempDir;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("owned channel workspace remains live")
    }
}

impl Drop for RetainedChannelProbeWorkspace {
    fn drop(&mut self) {
        if let Some(workspace) = self.0.take() {
            let retained = workspace.keep();
            // Diagnostic failure must not replace an original unwinding oracle.
            let _ = std::panic::catch_unwind(|| {
                let subscriber = tracing_subscriber::fmt()
                    .with_test_writer()
                    .with_max_level(tracing::Level::INFO)
                    .finish();
                tracing::subscriber::with_default(subscriber, || {
                    tracing::info!(path = %retained.display(), "retained owned channel probe workspace");
                });
            });
        }
    }
}

fn channel_turn_probe_principal(tmp: &TempDir, session_key: Option<String>, sender: Option<String>) -> MemoryPrincipal {
    MemoryPrincipal {
        require_owner_match: false,
        workspace_id: tmp.path().to_string_lossy().to_string(),
        agent_id: None,
        persona_id: None,
        session_key,
        channel: Some("test-channel".to_string()),
        sender,
        owner_id: None,
        legacy_session_key: None,
    }
}

/// D-053 / REDACT-TURN probe: a real channel turn (`process_channel_message`
/// -> `run_channel_turn`) whose provider replies with a secret reaches the
/// `Completed` `finalize_turn` commit and the assistant conversation turn;
/// nothing under the workspace may keep the secret, so this must fail
/// before the REDACT-TURN commits.
#[tokio::test]
async fn channel_turn_persists_no_secret_from_a_reply() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};

    let tmp = RetainedChannelProbeWorkspace::new();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let channel_impl = Arc::new(RecordingChannel::default());
    run_physical_channel_probe(&tmp, Arc::clone(&memory), channel_impl.clone(), false).await;

    // The turn really ran: the reply was delivered and the assistant turn
    // was recorded as a message event.
    {
        let sent_messages = channel_impl.sent_messages.lock().await;
        let responses = response_messages(&sent_messages);
        assert_eq!(responses.len(), 1, "the turn must deliver one reply: {sent_messages:?}");
        assert!(
            responses.iter().any(|response| response.contains("Here is the key")),
            "the provider reply must reach the channel: {responses:?}"
        );
    }
    let events = memory
        .list_message_events_since(&channel_turn_probe_principal(&tmp, None, None), 0, 20)
        .await
        .expect("test: list message events");
    let user_event = events
        .iter()
        .find(|event| event.role == "user")
        .expect("test: the inbound user event is recorded");
    let outbound_events = memory
        .list_message_events_since(
            &channel_turn_probe_principal(&tmp, user_event.session_key.clone(), Some("prx".to_string())),
            0,
            20,
        )
        .await
        .expect("test: list outbound message events");
    let assistant_event = outbound_events
        .iter()
        .find(|event| event.role == "assistant")
        .expect("test: the completed turn records an assistant event");
    assert!(
        !assistant_event.content.contains(PROBE_SECRET),
        "the assistant event must not carry the secret"
    );

    assert_nothing_under_persists_the_probe_secret(tmp.path());
}

/// D-053 / REDACT-TURN probe: a real channel turn (`process_channel_message`
/// -> `run_channel_turn`) whose provider fails with an error naming a secret
/// reaches the `Failed` `finalize_turn` commit; nothing under the workspace
/// may keep the secret, so this must fail before the REDACT-TURN commits.
#[tokio::test]
async fn channel_turn_persists_no_secret_from_a_provider_error() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};

    let tmp = RetainedChannelProbeWorkspace::new();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let channel_impl = Arc::new(RecordingChannel::default());
    run_physical_channel_probe(&tmp, Arc::clone(&memory), channel_impl.clone(), true).await;

    // The turn really ran and failed: the reader got exactly one notice
    // (which never quotes the error), and the inbound event was recorded.
    {
        let sent_messages = channel_impl.sent_messages.lock().await;
        let responses = response_messages(&sent_messages);
        assert_eq!(
            responses.len(),
            1,
            "a failed turn must still answer once: {sent_messages:?}"
        );
        assert!(
            responses.iter().all(|response| !response.contains(PROBE_SECRET)),
            "the failure notice must not quote the secret: {responses:?}"
        );
    }
    let events = memory
        .list_message_events_since(&channel_turn_probe_principal(&tmp, None, None), 0, 20)
        .await
        .expect("test: list message events");
    assert!(
        events.iter().any(|event| event.role == "user"),
        "the failed turn must still record its inbound event: {events:?}"
    );

    assert_nothing_under_persists_the_probe_secret(tmp.path());
}

/// Metadata precedes the owner gate; refusal must not manufacture a user event.
#[tokio::test]
async fn channel_input_owner_refusal_preserves_metadata_order_without_admitting_input() {
    use crate::runtime::turn_cleanup::CleanupTracker;
    for mode in ["missing", "closed", "failed"] {
        let tmp = RetainedChannelProbeWorkspace::new();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("owned SQLite"));
        let channel = Arc::new(RecordingChannel::default());
        let ctx = channel_turn_probe_ctx(&tmp, Arc::clone(&memory), Arc::new(DummyProvider), channel.clone());
        let message = gate_test_message();
        assert!(
            memory
                .get_chat_profile(&message.channel, &message.reply_target)
                .await
                .expect("profile before")
                .is_none()
        );
        let tracker = Arc::new(CleanupTracker::default());
        if mode == "missing" {
            assert!(crate::runtime::turn_cleanup::current_tracker().is_none());
            process_channel_message(ctx, message.clone(), CancellationToken::new()).await;
        } else {
            if mode == "closed" {
                tracker
                    .drain(tokio::time::Instant::now() + Duration::from_secs(15))
                    .await
                    .expect("close empty owner");
            } else {
                tracker
                    .register(CancellationToken::new())
                    .expect("actual ticket")
                    .finish(false);
            }
            tracker
                .scope(process_channel_message(ctx, message.clone(), CancellationToken::new()))
                .await;
        }
        assert!(
            memory
                .get_chat_profile(&message.channel, &message.reply_target)
                .await
                .expect("profile after")
                .is_some()
        );
        assert!(
            memory
                .list_message_events_since(&channel_turn_probe_principal(&tmp, None, None), 0, 20)
                .await
                .expect("owned event readback")
                .is_empty()
        );
        assert!(channel.sent_messages.lock().await.is_empty());
        let drained = tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(15))
            .await;
        assert_eq!(drained.is_err(), mode == "failed", "sticky failure is never reset");
    }
}
