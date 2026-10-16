#![cfg(test)]

use super::*;

/// A failed attempt is not handed back for re-execution.
///
/// The turn behind a webhook can call tools before the provider error that
/// ends it, and nothing here can tell whether it did. `RetryUnavailable`
/// existed for exactly this case but was unreachable while every release
/// path set `retry_eligible = true`; refusing the redelivery is what makes
/// it reachable, and it is the only answer that cannot duplicate a side
/// effect the first attempt already committed.
#[tokio::test]
async fn webhook_idempotency_failed_attempt_is_not_reexecuted() {
    let provider_impl = Arc::new(FailFirstProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let state = webhook_test_state(provider);
    owned_bound_gateway_fixture(state, |state| async move {
        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("retry-after-error"));

        let first = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            test_uri(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let retry = handle_webhook(
            State(state),
            test_connect_info(),
            test_uri(),
            headers,
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(retry.status(), StatusCode::CONFLICT);
        let payload = retry.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["status"], "retry_unavailable");
        assert_eq!(
            provider_impl.calls.load(Ordering::SeqCst),
            1,
            "a refused redelivery must not reach the provider again"
        );
    })
    .await;
}

/// The autosave key is derived from the idempotency digest, so it neither
/// leaks the caller's raw key nor changes between deliveries of the same
/// request. (Stability across digests is asserted directly in
/// `webhook_memory_key_is_stable_for_idempotent_retry`; this covers the key
/// actually written by the webhook path.)
#[tokio::test]
async fn webhook_idempotency_autosave_key_is_derived_not_leaked() {
    let provider_impl = Arc::new(FailFirstProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let tracking_impl = Arc::new(TrackingMemory::default());
    let mut state = webhook_test_state(provider);
    state.mem = tracking_impl.clone();
    state.auto_save = true;
    let mut headers = HeaderMap::new();
    headers.insert("X-Idempotency-Key", HeaderValue::from_static("stable-autosave"));

    let first = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from_static(br#"{"message":"remember this stable autosave payload across the retry attempt"}"#),
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let keys = tracking_impl.keys.lock();
    assert_eq!(keys.len(), 1);
    let key = keys.first().expect("autosave key recorded");
    assert!(key.starts_with("webhook_msg_"));
    assert!(!key.contains("stable-autosave"));
}

/// A cancelled *caller* is not a cancelled *run*. The turn lives in a
/// detached job, so aborting the handler task leaves the work going — and
/// the idempotency key must stay claimed while it does. Releasing it on
/// abandonment, as `IdempotencyClaim::drop` used to, would let the redelivery
/// re-execute a request that was still running and had already begun
/// committing side effects.
#[tokio::test]
async fn webhook_idempotency_cancelled_attempt_does_not_reexecute() {
    owned_blocking_webhook_fixture(|state, provider_impl, callers| async move {
        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("retry-after-cancel"));

        let started = provider_impl.first_started.notified();
        let first_state = state.clone();
        let first_headers = headers.clone();
        let first = callers.spawn(async move {
            handle_webhook(
                State(first_state),
                test_connect_info(),
                test_uri(),
                first_headers,
                Bytes::from_static(br#"{"message":"hello"}"#),
            )
            .await
            .into_response()
        });
        started.await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        // The abandoned caller must not have handed the key back for re-execution.
        let retry = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            test_uri(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(retry.status(), StatusCode::CONFLICT);
        let payload = retry.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["status"], "processing");
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);

        // Once the detached run finishes, the redelivery is answered from the
        // stored result instead of by running the request a second time.
        provider_impl.release_first.notify_one();
        let mut duplicate = None;
        for _ in 0..200_u32 {
            let response = handle_webhook(
                State(state.clone()),
                test_connect_info(),
                test_uri(),
                headers.clone(),
                Bytes::from_static(br#"{"message":"hello"}"#),
            )
            .await
            .into_response();
            if response.status() == StatusCode::OK {
                duplicate = Some(response);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let duplicate = duplicate.expect("detached run should commit its idempotent result");
        let payload = duplicate.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["status"], "duplicate");
        assert_eq!(parsed["response"], "released");
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    })
    .await;
}

#[tokio::test]
async fn webhook_idempotency_concurrent_attempt_reports_processing() {
    owned_blocking_webhook_fixture(|state, provider_impl, callers| async move {
        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("concurrent-key"));

        let started = provider_impl.first_started.notified();
        let first_state = state.clone();
        let first_headers = headers.clone();
        let first = callers.spawn(async move {
            handle_webhook(
                State(first_state),
                test_connect_info(),
                test_uri(),
                first_headers,
                Bytes::from_static(br#"{"message":"hello"}"#),
            )
            .await
            .into_response()
        });
        started.await;

        let concurrent = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            test_uri(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        let concurrent_status = concurrent.status();
        let payload = concurrent.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();

        provider_impl.release_first.notify_one();
        let first_response = first.await.unwrap();
        assert_eq!(first_response.status(), StatusCode::OK);
        assert_eq!(concurrent_status, StatusCode::CONFLICT);
        assert_eq!(parsed["status"], "processing");
        assert_eq!(parsed["idempotent"], true);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    })
    .await;
}

#[tokio::test]
async fn webhook_idempotency_full_capacity_returns_service_unavailable() {
    owned_blocking_webhook_fixture(|mut state, provider_impl, callers| async move {
        state.idempotency_store = Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1));

        let mut first_headers = HeaderMap::new();
        first_headers.insert("X-Idempotency-Key", HeaderValue::from_static("first-key"));
        let started = provider_impl.first_started.notified();
        let first_state = state.clone();
        let first = callers.spawn(async move {
            handle_webhook(
                State(first_state),
                test_connect_info(),
                test_uri(),
                first_headers,
                Bytes::from_static(br#"{"message":"first"}"#),
            )
            .await
            .into_response()
        });
        started.await;

        let mut second_headers = HeaderMap::new();
        second_headers.insert("X-Idempotency-Key", HeaderValue::from_static("second-key"));
        let at_capacity = handle_webhook(
            State(state),
            test_connect_info(),
            test_uri(),
            second_headers,
            Bytes::from_static(br#"{"message":"second"}"#),
        )
        .await
        .into_response();
        assert_eq!(at_capacity.status(), StatusCode::SERVICE_UNAVAILABLE);
        let payload = at_capacity.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["retryable"], true);

        provider_impl.release_first.notify_one();
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    })
    .await;
}

#[tokio::test]
async fn webhook_idempotency_same_key_different_body_conflicts() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let mut state = webhook_test_state(provider);
    install_webhook_turn_runtime(&mut state);
    let mut headers = HeaderMap::new();
    headers.insert("X-Idempotency-Key", HeaderValue::from_static("body-bound-key"));

    let first = handle_webhook(
        State(state.clone()),
        test_connect_info(),
        test_uri(),
        headers.clone(),
        Bytes::from_static(br#"{"message":"first"}"#),
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::OK);

    let conflict = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from_static(br#"{"message":"second"}"#),
    )
    .await
    .into_response();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let payload = conflict.into_body().collect().await.unwrap().to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(parsed["status"], "request_conflict");
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn webhook_idempotency_rejects_oversize_key() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let state = webhook_test_state(provider);
    let mut headers = HeaderMap::new();
    let oversized_key = vec![b'x'; IDEMPOTENCY_MAX_KEY_BYTES + 1];
    headers.insert("X-Idempotency-Key", HeaderValue::from_bytes(&oversized_key).unwrap());

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from_static(br#"{"message":"hello"}"#),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn webhook_idempotency_oversize_success_is_not_reexecuted() {
    let provider_impl = Arc::new(OversizeResponseProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let mut state = webhook_test_state(provider);
    install_webhook_turn_runtime(&mut state);
    let mut headers = HeaderMap::new();
    headers.insert("X-Idempotency-Key", HeaderValue::from_static("oversize-result"));

    let first = handle_webhook(
        State(state.clone()),
        test_connect_info(),
        test_uri(),
        headers.clone(),
        Bytes::from_static(br#"{"message":"hello"}"#),
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::OK);

    let duplicate = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from_static(br#"{"message":"hello"}"#),
    )
    .await
    .into_response();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    let payload = duplicate.into_body().collect().await.unwrap().to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(parsed["status"], "replay_unavailable");
    assert!(parsed["response_id"].as_str().is_some());
    assert!(parsed["result_hash"].as_str().is_some());
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn webhook_records_gateway_message_events() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = webhook_test_config();
    config.workspace_dir = tmp.path().to_path_buf();
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

    let mut state = AppState {
        config: crate::config::new_shared(config),
        provider,
        model: "test-model".into(),
        temperature: 0.0,
        mem: Arc::clone(&memory),
        auto_save: false,
        tools_registry: Arc::new(vec![]),
        mcp_tool: None,
        turn_runtime: None,
        hooks: Arc::new(HookManager::new(tmp.path().to_path_buf())),
        webhook_token_hash: None,
        webhook_signing_secret: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
        whatsapp: None,
        signal: None,
        whatsapp_app_secret: None,
        linq: None,
        linq_signing_secret: None,
        nextcloud_talk: None,
        nextcloud_talk_webhook_secret: None,
        observer: Arc::new(crate::observability::NoopObserver),
        start_time: Instant::now(),
        gateway_port: 0,
        logs_broadcast_tx: broadcast::channel(16).0,
        #[cfg(feature = "wasm-plugins")]
        plugin_runtime: None,
    };
    install_webhook_turn_runtime(&mut state);

    let mut headers = HeaderMap::new();
    headers.insert("X-Idempotency-Key", HeaderValue::from_static("gateway-event-1"));
    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from_static(br#"{"message":"hello gateway","reply_target":"client-a"}"#),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    // D-025: a `/webhook` turn's durable session_key is credential-qualified
    // (`gateway:webhook:{credential}:{reply_target}`; `public` here, the
    // gateway requires no credential). Recalling by it (what the production
    // envelope reads) returns the request, shared terminal projections, and
    // provider telemetry for the turn.
    let canonical_principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: tmp.path().to_string_lossy().to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("gateway:webhook:public:client-a".to_string()),
        channel: Some("webhook".to_string()),
        sender: Some("client-a".to_string()),
        owner_id: None,
        legacy_session_key: None,
    };
    let events = memory
        .list_message_events_since(&canonical_principal, 0, 50)
        .await
        .unwrap();

    let user_event = events
        .iter()
        .find(|event| event.role == "user")
        .expect("gateway user event");
    let assistant_event = events
        .iter()
        .find(|event| event.role == "assistant")
        .expect("gateway assistant projection");
    assert_eq!(user_event.source, "gateway");
    assert_eq!(user_event.channel.as_deref(), Some("webhook"));
    assert_eq!(user_event.content, "hello gateway");
    assert_eq!(
        user_event.session_key.as_deref(),
        Some("gateway:webhook:public:client-a")
    );
    let expected_digest = webhook_idempotency_digest("public", "gateway-event-1");
    assert_eq!(
        user_event.idempotency_key.as_deref(),
        Some(format!("gateway:webhook:{expected_digest}").as_str())
    );
    assert!(
        !user_event
            .idempotency_key
            .as_deref()
            .expect("idempotency digest")
            .contains("gateway-event-1")
    );
    assert_eq!(assistant_event.content, "ok");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "provider.final_outcome")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn.finalized")
            .count(),
        1
    );
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);

    // D-025: neither pre-upgrade key reaches the credential-qualified
    // session, and the envelope carries no legacy key to read-merge: history
    // under `gateway:webhook:{reply_target}` cannot be attributed to a
    // credential, so continuity restarts once.
    for old_key in ["gateway:webhook:client-a", "gateway:webhook:client-a:prx"] {
        let old_principal = MemoryPrincipal {
            session_key: Some(old_key.to_string()),
            ..canonical_principal.clone()
        };
        assert!(
            memory
                .list_message_events_since(&old_principal, 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "{old_key}"
        );
    }
    assert!(
        events
            .iter()
            .all(|event| event.session_key.as_deref() == Some("gateway:webhook:public:client-a"))
    );
}

// ══════════════════════════════════════════════════════════
// IdempotencyStore Edge-Case Tests
// ══════════════════════════════════════════════════════════

#[test]
fn idempotency_store_allows_different_keys() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(1), 100));
    let claims = ["key-a", "key-b", "key-c", "key-d"]
        .into_iter()
        .enumerate()
        .map(|(index, key)| acquire_idempotency_claim(&store, key, test_fingerprint(index as u8)))
        .collect::<Vec<_>>();

    assert_eq!(claims.len(), 4);
}

#[test]
fn idempotency_store_max_keys_clamped_to_one() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(1), 0));
    let _claim = acquire_idempotency_claim(&store, "only-key", test_fingerprint(1));
    assert!(matches!(
        store.claim("second-key".to_string(), test_fingerprint(2)),
        IdempotencyClaimOutcome::AtCapacity
    ));
}

#[test]
fn idempotency_store_same_key_different_body_conflicts() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(5), 100));
    let _claim = acquire_idempotency_claim(&store, "rapid", test_fingerprint(1));
    assert!(matches!(
        store.claim("rapid".to_string(), test_fingerprint(2)),
        IdempotencyClaimOutcome::RequestConflict
    ));
}

#[test]
fn idempotency_store_processing_survives_ttl_and_failed_retry_expires() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_millis(1), 100));
    let claim = acquire_idempotency_claim(&store, "ttl-key", test_fingerprint(1));
    std::thread::sleep(Duration::from_millis(10));
    assert!(matches!(
        store.claim("ttl-key".to_string(), test_fingerprint(1)),
        IdempotencyClaimOutcome::Processing
    ));

    assert!(claim.fail());
    std::thread::sleep(Duration::from_millis(10));
    let _retry = acquire_idempotency_claim(&store, "ttl-key", test_fingerprint(1));
}

/// A claim abandoned *after* the attempt began doing observable work leaves
/// the key non-retryable: nothing here knows how far the run got, and a
/// redelivery that re-executed it would duplicate whatever it did commit.
#[test]
fn idempotency_abandoned_claim_is_not_retryable_once_dispatched() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(5), 100));
    let mut claim = acquire_idempotency_claim(&store, "dispatched", test_fingerprint(1));
    claim.mark_dispatched();
    drop(claim);

    assert!(matches!(
        store.claim("dispatched".to_string(), test_fingerprint(1)),
        IdempotencyClaimOutcome::RetryUnavailable
    ));
}

/// The same abandonment *before* dispatch is provably a no-op, so the key
/// stays retryable — the tightened rule blocks unknown side effects, it does
/// not block every retry.
#[test]
fn idempotency_abandoned_claim_stays_retryable_before_dispatch() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(5), 100));
    drop(acquire_idempotency_claim(&store, "undispatched", test_fingerprint(1)));

    assert!(matches!(
        store.claim("undispatched".to_string(), test_fingerprint(1)),
        IdempotencyClaimOutcome::Acquired(_)
    ));
}

/// Explicit failure follows the same rule as abandonment: a run that had
/// started working is not offered back for re-execution.
#[test]
fn idempotency_explicit_failure_after_dispatch_is_not_retryable() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(5), 100));
    let mut claim = acquire_idempotency_claim(&store, "failed", test_fingerprint(1));
    claim.mark_dispatched();
    assert!(claim.fail());

    assert!(matches!(
        store.claim("failed".to_string(), test_fingerprint(1)),
        IdempotencyClaimOutcome::RetryUnavailable
    ));
}

#[test]
fn idempotency_store_terminal_ttl_starts_at_completion() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_millis(20), 1));
    let claim = acquire_idempotency_claim(&store, "old-key", test_fingerprint(1));
    std::thread::sleep(Duration::from_millis(15));
    assert!(claim.succeed(test_idempotency_replay("ok"), "hash".to_string()));
    std::thread::sleep(Duration::from_millis(10));
    assert!(matches!(
        store.claim("old-key".to_string(), test_fingerprint(1)),
        IdempotencyClaimOutcome::Replay(_)
    ));
    std::thread::sleep(Duration::from_millis(15));
    let _retry = acquire_idempotency_claim(&store, "old-key", test_fingerprint(1));
}

#[test]
fn idempotency_store_stale_generation_cannot_finish_new_attempt() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(1), 1));
    let first = acquire_idempotency_claim(&store, "key", test_fingerprint(1));
    let first_generation = first.generation;
    assert!(first.fail());
    let second = acquire_idempotency_claim(&store, "key", test_fingerprint(1));

    assert!(!store.complete_if_owner(
        "key",
        first_generation,
        test_idempotency_replay("stale"),
        "stale-hash".to_string(),
    ));
    assert!(matches!(
        store.claim("key".to_string(), test_fingerprint(1)),
        IdempotencyClaimOutcome::Processing
    ));
    assert!(second.succeed(test_idempotency_replay("fresh"), "fresh-hash".to_string()));
}

#[test]
fn idempotency_store_payload_budget_is_released_after_failure() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(1), 100));
    let mut claims = (0..(IDEMPOTENCY_REPLAY_BUDGET_BYTES / IDEMPOTENCY_MAX_REPLAY_BYTES))
        .map(|index| acquire_idempotency_claim(&store, &format!("key-{index}"), test_fingerprint(index as u8)))
        .collect::<Vec<_>>();
    assert!(matches!(
        store.claim("over-budget".to_string(), test_fingerprint(250)),
        IdempotencyClaimOutcome::AtCapacity
    ));

    assert!(claims.pop().expect("one claim").fail());
    let _replacement = acquire_idempotency_claim(&store, "replacement", test_fingerprint(251));
}

#[test]
fn idempotency_store_oversize_success_keeps_non_reexecuting_tombstone() {
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(1), 1));
    let claim = acquire_idempotency_claim(&store, "oversize", test_fingerprint(1));
    let replay = IdempotencyReplay {
        response_id: Uuid::new_v4(),
        response: Arc::from("x".repeat(IDEMPOTENCY_MAX_REPLAY_BYTES + 1)),
        model: Arc::from("test-model"),
    };
    let response_id = replay.response_id;
    assert!(claim.succeed(replay, "oversize-hash".to_string()));

    assert!(matches!(
        store.claim("oversize".to_string(), test_fingerprint(1)),
        IdempotencyClaimOutcome::ReplayUnavailable {
            response_id: id,
            result_hash,
        } if id == response_id && result_hash == "oversize-hash"
    ));
}

/// S2.5 P1-A: the /metrics endpoint carries the chat counters in PrometheusObserver mode.
#[tokio::test]
async fn s2_5_p1c_metrics_endpoint_exposes_chat_counters() {
    let prom = Arc::new(crate::observability::PrometheusObserver::try_new().unwrap());
    crate::observability::Observer::record_event(prom.as_ref(), &crate::observability::ObserverEvent::HeartbeatTick);
    // Bump all four chat counters, with labels unique to this test so other
    // tests' accumulated values cannot interfere.
    crate::observability::chat_metrics::inc_action("s2_5_p1c_test_kind");
    crate::observability::chat_metrics::inc_effect("s2_5_p1c_test_effect");
    crate::observability::chat_metrics::inc_stream_chunk();
    crate::observability::chat_metrics::inc_dispatch_drop("s2_5_p1c_test_drop");

    let observer: Arc<dyn crate::observability::Observer> = prom;
    let state = AppState {
        config: crate::config::new_shared(Config::default()),
        provider: Arc::new(MockProvider::default()),
        model: "test-model".into(),
        temperature: 0.0,
        mem: Arc::new(MockMemory),
        auto_save: false,
        tools_registry: Arc::new(vec![]),
        mcp_tool: None,
        turn_runtime: None,
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
        webhook_token_hash: None,
        webhook_signing_secret: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
        whatsapp: None,
        signal: None,
        whatsapp_app_secret: None,
        linq: None,
        linq_signing_secret: None,
        nextcloud_talk: None,
        nextcloud_talk_webhook_secret: None,
        observer,
        start_time: Instant::now(),
        gateway_port: 0,
        logs_broadcast_tx: broadcast::channel(16).0,
        #[cfg(feature = "wasm-plugins")]
        plugin_runtime: None,
    };

    let response = handle_metrics(State(state)).await.into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    // The observer's own metrics are still there.
    assert!(
        text.contains("prx_heartbeat_ticks_total"),
        "observer metrics must be present"
    );
    // All four chat counter names must appear.
    assert!(text.contains("prx_chat_actions_total"), "chat actions counter missing");
    assert!(text.contains("prx_chat_effects_total"), "chat effects counter missing");
    assert!(
        text.contains("prx_chat_stream_chunks_total"),
        "chat stream chunks counter missing"
    );
    assert!(
        text.contains("prx_chat_dispatch_drops_total"),
        "chat dispatch drops counter missing"
    );
}
