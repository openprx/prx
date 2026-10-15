#![cfg(test)]

use super::*;

#[tokio::test]
async fn webhook_token_hash_rejects_missing_header() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);
    let secret = generate_test_secret();

    let state = AppState {
        config: crate::config::new_shared(Config::default()),
        provider,
        model: "test-model".into(),
        temperature: 0.0,
        mem: memory,
        auto_save: false,
        tools_registry: Arc::new(vec![]),
        mcp_tool: None,
        turn_runtime: None,
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
        webhook_token_hash: Some(Arc::from(hash_webhook_secret(&secret))),
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

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        HeaderMap::new(),
        Bytes::from_static(br#"{"message":"hello"}"#),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn webhook_token_hash_rejects_invalid_header() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);
    let valid_secret = generate_test_secret();
    let wrong_secret = generate_test_secret();

    let state = AppState {
        config: crate::config::new_shared(Config::default()),
        provider,
        model: "test-model".into(),
        temperature: 0.0,
        mem: memory,
        auto_save: false,
        tools_registry: Arc::new(vec![]),
        mcp_tool: None,
        turn_runtime: None,
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
        webhook_token_hash: Some(Arc::from(hash_webhook_secret(&valid_secret))),
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

    let mut headers = HeaderMap::new();
    headers.insert("X-Webhook-Token", HeaderValue::from_str(&wrong_secret).unwrap());

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from_static(br#"{"message":"hello"}"#),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn webhook_token_hash_accepts_valid_header() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let secret = generate_test_secret();

    let mut state = webhook_test_state(provider);
    state.webhook_token_hash = Some(Arc::from(hash_webhook_secret(&secret)));
    install_webhook_turn_runtime(&mut state);

    let mut headers = HeaderMap::new();
    headers.insert("X-Webhook-Token", HeaderValue::from_str(&secret).unwrap());

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from_static(br#"{"message":"hello"}"#),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn webhook_signature_rejects_missing_header() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);
    let secret = generate_test_secret();

    let state = AppState {
        config: crate::config::new_shared(Config::default()),
        provider,
        model: "test-model".into(),
        temperature: 0.0,
        mem: memory,
        auto_save: false,
        tools_registry: Arc::new(vec![]),
        mcp_tool: None,
        turn_runtime: None,
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
        webhook_token_hash: None,
        webhook_signing_secret: Some(Arc::from(secret)),
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

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        HeaderMap::new(),
        Bytes::from_static(br#"{"message":"hello"}"#),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn webhook_signature_accepts_valid_hmac() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let secret = generate_test_secret();
    let body = br#"{"message":"hello"}"#;

    let mut state = webhook_test_state(provider);
    state.webhook_signing_secret = Some(Arc::from(secret.clone()));

    owned_bound_gateway_fixture(state, |state| async move {
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Webhook-Signature",
            HeaderValue::from_str(&format!("sha256={}", compute_whatsapp_signature_hex(&secret, body))).unwrap(),
        );

        let response = handle_webhook(
            State(state),
            test_connect_info(),
            test_uri(),
            headers,
            Bytes::from_static(body),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    })
    .await;
}

#[tokio::test]
async fn nextcloud_talk_webhook_returns_not_found_when_not_configured() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::default());
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);

    let state = AppState {
        config: crate::config::new_shared(Config::default()),
        provider,
        model: "test-model".into(),
        temperature: 0.0,
        mem: memory,
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

    let response = handle_nextcloud_talk_webhook(
        State(state),
        HeaderMap::new(),
        Bytes::from_static(br#"{"type":"message"}"#),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn nextcloud_talk_webhook_rejects_invalid_signature() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);

    let channel = Arc::new(NextcloudTalkChannel::new(
        "https://cloud.example.com".into(),
        "app-token".into(),
        vec!["*".into()],
    ));

    let secret = "nextcloud-test-secret";
    let random = "seed-value";
    let body = r#"{"type":"message","object":{"token":"room-token"},"message":{"actorType":"users","actorId":"user_a","message":"hello"}}"#;
    let _valid_signature = compute_nextcloud_signature_hex(secret, random, body);
    let invalid_signature = "deadbeef";

    let state = AppState {
        config: crate::config::new_shared(Config::default()),
        provider,
        model: "test-model".into(),
        temperature: 0.0,
        mem: memory,
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
        nextcloud_talk: Some(channel),
        nextcloud_talk_webhook_secret: Some(Arc::from(secret)),
        observer: Arc::new(crate::observability::NoopObserver),
        start_time: Instant::now(),
        gateway_port: 0,
        logs_broadcast_tx: broadcast::channel(16).0,
        #[cfg(feature = "wasm-plugins")]
        plugin_runtime: None,
    };

    let mut headers = HeaderMap::new();
    headers.insert("X-Nextcloud-Talk-Random", HeaderValue::from_str(random).unwrap());
    headers.insert(
        "X-Nextcloud-Talk-Signature",
        HeaderValue::from_str(invalid_signature).unwrap(),
    );

    let response = handle_nextcloud_talk_webhook(State(state), headers, Bytes::from(body))
        .await
        .into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
}
