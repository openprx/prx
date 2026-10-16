#![cfg(test)]

use super::*;

#[tokio::test]
async fn metrics_endpoint_renders_prometheus_output() {
    let prom = Arc::new(crate::observability::PrometheusObserver::try_new().unwrap());
    crate::observability::Observer::record_event(prom.as_ref(), &crate::observability::ObserverEvent::HeartbeatTick);

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
    assert!(text.contains("prx_heartbeat_ticks_total 1"));
}

#[test]
fn gateway_rate_limiter_blocks_after_limit() {
    let limiter = GatewayRateLimiter::new(2, 2, 2, 100);
    assert!(limiter.allow_pair("127.0.0.1"));
    assert!(limiter.allow_pair("127.0.0.1"));
    assert!(!limiter.allow_pair("127.0.0.1"));
}

/// S2.5 P1-A: the /metrics endpoint still exposes the chat counters in NoopObserver mode.
#[tokio::test]
async fn s2_5_p1c_metrics_endpoint_exposes_chat_when_observer_noop() {
    // Force the LazyLock initialisation so all four counters register in CHAT_REGISTRY.
    crate::observability::chat_metrics::inc_action("s2_5_p1c_noop_kind");
    crate::observability::chat_metrics::inc_effect("s2_5_p1c_noop_effect");
    crate::observability::chat_metrics::inc_stream_chunk();
    crate::observability::chat_metrics::inc_dispatch_drop("s2_5_p1c_noop_drop");

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
        observer: Arc::new(crate::observability::NoopObserver),
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
    // The noop-observer fallback path still carries the hint prefix.
    assert!(
        text.contains("Prometheus backend not enabled"),
        "hint prefix must be present"
    );
    // The chat counter names must appear too, even with a noop observer.
    assert!(
        text.contains("prx_chat_actions_total"),
        "chat actions counter missing with noop observer"
    );
    assert!(
        text.contains("prx_chat_effects_total"),
        "chat effects counter missing with noop observer"
    );
    assert!(
        text.contains("prx_chat_stream_chunks_total"),
        "chat stream chunks counter missing with noop observer"
    );
    assert!(
        text.contains("prx_chat_dispatch_drops_total"),
        "chat dispatch drops counter missing with noop observer"
    );
}

#[test]
fn readiness_http_is_non_success_without_ready_required_components() {
    let runtime = crate::health::HealthSnapshot {
        pid: 1,
        updated_at: chrono::Utc::now().to_rfc3339(),
        uptime_seconds: 1,
        components: std::collections::BTreeMap::new(),
    };

    let (status, Json(body)) = health_response(false, runtime);

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "not_ready");
}
