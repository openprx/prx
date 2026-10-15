#![cfg(test)]

use super::*;

#[test]
fn webhook_memory_key_is_stable_for_idempotent_retry() {
    let key1 = webhook_memory_key(Some("digest"));
    let key2 = webhook_memory_key(Some("digest"));

    assert_eq!(key1, key2);
    assert_eq!(key1, "webhook_msg_digest");
}

#[test]
fn webhook_idempotency_digest_is_scoped_and_hides_raw_key() {
    let first_scope = webhook_idempotency_digest("scope-a", "sensitive-external-key");
    let second_scope = webhook_idempotency_digest("scope-b", "sensitive-external-key");

    assert_eq!(first_scope.len(), 64);
    assert!(first_scope.chars().all(|character| character.is_ascii_hexdigit()));
    assert!(!first_scope.contains("sensitive-external-key"));
    assert_ne!(first_scope, second_scope);
}

#[test]
fn whatsapp_memory_key_includes_sender_and_message_id() {
    let msg = ChannelMessage {
        id: "wamid-123".into(),
        sender: "+1234567890".into(),
        reply_target: "+1234567890".into(),
        content: "hello".into(),
        channel: "whatsapp".into(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };

    let key = whatsapp_memory_key(&msg);
    assert_eq!(key, "whatsapp_+1234567890_wamid-123");
}

/// Records the outbound origin channel visible from inside a turn.
#[derive(Default)]
struct OriginRecordingProvider {
    calls: AtomicUsize,
    origin: parking_lot::Mutex<Option<String>>,
}

#[async_trait]
impl Provider for OriginRecordingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.origin.lock() =
            crate::tools::message_send::current_outbound_origin_channel().map(|channel| channel.to_string());
        Ok("ok".into())
    }
}

#[tokio::test]
async fn webhook_default_hint_uses_only_selected_route_key_on_wire() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
    let workspace = tempfile::tempdir().expect("test: temp workspace");
    let root = CredentialProbe::start().await;
    let route = CredentialProbe::start().await;
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.config_path = workspace.path().join("config.toml");
    config.default_provider = Some(root.custom_provider());
    config.default_model = Some("hint:fast".to_string());
    config.api_key = Some("PRX-WEBHOOK-ROOT-KEY".to_string());
    let route_provider = route.custom_provider();
    config.cost.prices.insert(
        format!("{route_provider}/{PROBE_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: route_provider,
        model: PROBE_MODEL.to_string(),
        api_key: Some("PRX-WEBHOOK-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    let options = providers::provider_runtime_options_from_config(&config);
    let (provider, model, provider_binding) =
        build_gateway_turn_provider(&config, &options).expect("route provider builds");
    let mut state = webhook_test_state(provider);
    state.model = model;
    state.config = crate::config::new_shared(config);
    state.mem = Arc::new(SqliteMemory::new(workspace.path()).expect("route fixture sqlite"));
    state.hooks = Arc::new(HookManager::new(workspace.path().to_path_buf()));
    let generation = state.config.pin();
    let security = crate::runtime::bootstrap::build_security_policy(&generation.effective);
    let runtime = TurnRuntimeGeneration {
        provider_binding,
        decision_slot: Arc::new(decision_runtime::GatewayDecisionSlot::new(security, 0)),
        config_generation: generation,
        provider: Arc::clone(&state.provider),
        model: state.model.clone(),
        temperature: state.temperature,
        tools_registry: Arc::clone(&state.tools_registry),
        mcp_tool: state.mcp_tool.clone(),
    };
    state.turn_runtime = Some(Arc::new(GatewayTurnRuntimeOwner::new(
        runtime,
        Arc::clone(&state.config),
        Arc::clone(&state.mem),
        state.signal.clone(),
        state.whatsapp.clone(),
        state.linq.clone(),
        state.nextcloud_talk.clone(),
        Arc::clone(&state.hooks),
        #[cfg(feature = "wasm-plugins")]
        state.plugin_runtime.clone(),
    )));
    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        HeaderMap::new(),
        Bytes::from_static(br#"{"message":"probe"}"#),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(route.wait_for_requests(1, Duration::from_secs(10)).await, 1);
    assert!(route.saw("PRX-WEBHOOK-ROUTE-KEY"));
    assert!(!route.saw("PRX-WEBHOOK-ROOT-KEY"));
    assert!(root.requests().is_empty());
}

/// R3: a webhook turn owns no channel of its own, so `message_send` falls
/// back to whichever handle the daemon registered — a Signal handle in
/// production. The turn must therefore name its real origin, or every
/// outbound scope rule written for this surface is unreachable and every
/// rule written for Signal is applied to it by accident.
#[tokio::test]
async fn a_webhook_turn_names_its_origin_channel_for_outbound_scope() {
    let provider_impl = Arc::new(OriginRecordingProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let mut state = webhook_test_state(provider);
    install_webhook_turn_runtime(&mut state);

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        HeaderMap::new(),
        Bytes::from_static(br#"{"message":"hello"}"#),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        provider_impl.calls.load(Ordering::SeqCst),
        1,
        "the turn must have reached the provider"
    );
    let observed = provider_impl.origin.lock().clone();
    assert_eq!(
        observed,
        Some("webhook".to_string()),
        "the turn must run with its own origin channel in scope"
    );
}

/// C2b-2: a tool-schema refusal reaches the authenticated webhook caller
/// as a stable code with every refused tool, not the generic
/// "LLM request failed" that told the caller nothing it could act on.
#[tokio::test]
async fn a_webhook_tool_schema_refusal_returns_a_stable_code_and_the_tools() {
    use futures_util::FutureExt as _;

    struct CountingRefusal {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for CountingRefusal {
        async fn chat_with_system(
            &self,
            system_prompt: Option<&str>,
            message: &str,
            model: &str,
            temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            crate::providers::tool_schema_refusal_tests::RefusingProvider
                .chat_with_system(system_prompt, message, model, temperature)
                .await
        }
    }

    // Retain this owned fixture's SQLite and key evidence on success and panic.
    let scratch = tempfile::tempdir().expect("schema fixture directory").keep();
    let subscriber = tracing_subscriber::fmt()
        .with_test_writer()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(path = %scratch.display(), "retained schema fixture workspace");
    });
    let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
        .await
        .expect("owned schema decision listener")
        .retain_evidence_at(scratch.clone());
    let endpoint = driver.endpoint().to_owned();
    driver
        .run(true, async move {
            let mut config = Config::default();
            config.workspace_dir = scratch.join("workspace");
            config.config_path = scratch.join("config.toml");
            config.default_provider = Some("ollama".to_string());
            config.default_model = Some("prx-schema-fixture".to_string());
            config.memory.backend = "sqlite".to_string();
            config.providers.models.push(
                toml::from_str("provider = \"ollama\"\nmodel = \"prx-schema-fixture\"\noperations = [\"chat\"]\n")
                    .expect("schema fixture chat declaration"),
            );
            config.cost.prices.insert(
                "ollama/prx-schema-fixture".to_string(),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 1.0,
                    cache_write: 1.0,
                    cache_read: 1.0,
                },
            );
            assert!(matches!(
                crate::llm::route_decision::usage_cost_resolution(
                    "ollama", "prx-schema-fixture", 1, 0, 0, 0, &config.cost,
                ),
                crate::llm::route_decision::UsagePrice::Priced(cost) if cost > 0.0
            ));
            crate::agent::agent::native_decision_fixture::configure(&mut config, &endpoint)
                .expect("schema fixture physical decision endpoint");
            let resolved = providers::selection::check_default(&config, providers::catalog::UnknownPolicy::Invocation)
                .expect("schema fixture PMM chat selection");
            let model = resolved.model().to_owned();
            let security = Arc::new(crate::security::SecurityPolicy::from_config(
                &config.autonomy,
                &config.workspace_dir,
            ));
            let memory: Arc<dyn Memory> = Arc::new(
                crate::memory::SqliteMemory::new(&config.workspace_dir).expect("schema fixture durable SQLite"),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let provider: Arc<dyn Provider> = Arc::new(CountingRefusal {
                calls: Arc::clone(&calls),
            });
            let mut state = webhook_test_state(Arc::clone(&provider));
            state.config = crate::config::new_shared(config);
            state.model = model.clone();
            state.mem = Arc::clone(&memory);
            state.hooks = Arc::new(HookManager::new(scratch.clone()));
            // Controlled trait injection: PMM selection and pointer identity are real;
            // this does not claim the production provider factory created the fixture.
            let runtime = TurnRuntimeGeneration {
                provider_binding: Some(Arc::new(decision_source::GatewayProviderBinding::new(
                    Arc::clone(&provider),
                    resolved,
                ))),
                decision_slot: Arc::new(decision_runtime::GatewayDecisionSlot::new(security, 0)),
                config_generation: state.config.pin(),
                provider,
                model,
                temperature: state.temperature,
                tools_registry: Arc::clone(&state.tools_registry),
                mcp_tool: None,
            };
            let cleanup = Arc::clone(&runtime.decision_slot.cleanup);
            let owner = Arc::new(GatewayTurnRuntimeOwner::new(
                runtime,
                Arc::clone(&state.config),
                memory,
                None,
                None,
                None,
                None,
                Arc::clone(&state.hooks),
                #[cfg(feature = "wasm-plugins")]
                None,
            ));
            state.turn_runtime = Some(Arc::clone(&owner));
            let work = Box::pin(async move {
                let response = handle_webhook(
                    State(state),
                    test_connect_info(),
                    test_uri(),
                    HeaderMap::new(),
                    Bytes::from_static(br#"{"message":"hello"}"#),
                )
                .await
                .into_response();
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
                let payload = response.into_body().collect().await.unwrap().to_bytes();
                let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                assert_eq!(parsed["error"], "tool_schema_refused", "{parsed}");
                assert_eq!(
                    parsed["tools"],
                    serde_json::json!(["fixture_conditional", "fixture_alternatives"]),
                    "{parsed}"
                );
                assert_eq!(calls.load(Ordering::SeqCst), 1, "actual refusal provider entered once");
            });
            let outcome = cleanup.scope(std::panic::AssertUnwindSafe(work).catch_unwind()).await;
            // All request futures and state references are gone before owner retirement.
            drop(owner);
            let drained = cleanup
                .drain(tokio::time::Instant::now() + Duration::from_secs(15))
                .await;
            if let Err(error) = drained {
                let subscriber = tracing_subscriber::fmt()
                    .with_test_writer()
                    .with_ansi(false)
                    .without_time()
                    .with_max_level(tracing::Level::ERROR)
                    .finish();
                tracing::subscriber::with_default(subscriber, || {
                    tracing::error!("schema fixture cleanup failed: {error}");
                });
                if let Err(panic) = outcome {
                    std::panic::resume_unwind(panic);
                }
                panic!("schema fixture cleanup failed: {error}");
            }
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        })
        .await;
}

#[tokio::test]
async fn webhook_idempotency_skips_duplicate_provider_calls() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let mut state = webhook_test_state(provider);
    install_webhook_turn_runtime(&mut state);

    let mut headers = HeaderMap::new();
    headers.insert("X-Idempotency-Key", HeaderValue::from_static("abc-123"));

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
    let first_payload = first.into_body().collect().await.unwrap().to_bytes();
    let first_json: serde_json::Value = serde_json::from_slice(&first_payload).unwrap();
    let response_id = first_json["response_id"]
        .as_str()
        .expect("successful idempotent request must expose a stable response identity")
        .to_string();

    let second = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from_static(br#"{"message":"hello"}"#),
    )
    .await
    .into_response();
    assert_eq!(second.status(), StatusCode::OK);

    let payload = second.into_body().collect().await.unwrap().to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(parsed["status"], "duplicate");
    assert_eq!(parsed["idempotent"], true);
    assert_eq!(parsed["response"], "ok");
    assert_eq!(parsed["model"], "test-model");
    assert_eq!(parsed["response_id"], response_id);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn webhook_autosave_stores_distinct_keys_per_request() {
    let workspace = tempfile::TempDir::new().expect("test: autosave workspace");
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let sqlite = Arc::new(SqliteMemory::new(workspace.path()).expect("test: autosave sqlite"));
    let memory: Arc<dyn Memory> = sqlite.clone();
    let mut config = webhook_test_config();
    config.workspace_dir = workspace.path().to_path_buf();

    let mut state = AppState {
        config: crate::config::new_shared(config),
        provider,
        model: "test-model".into(),
        temperature: 0.0,
        mem: memory,
        auto_save: true,
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
    install_webhook_turn_runtime(&mut state);

    let headers = HeaderMap::new();

    let first = handle_webhook(
        State(state.clone()),
        test_connect_info(),
        test_uri(),
        headers.clone(),
        Bytes::from_static(br#"{"message":"Remember webhook autosave message number one for later"}"#),
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::OK);

    let second = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from_static(br#"{"message":"Remember webhook autosave message number two for later"}"#),
    )
    .await
    .into_response();
    assert_eq!(second.status(), StatusCode::OK);

    let keys = sqlite
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .expect("test: list autosaved messages")
        .into_iter()
        .map(|entry| entry.key)
        .collect::<Vec<_>>();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
    assert!(keys[0].starts_with("webhook_msg_"));
    assert!(keys[1].starts_with("webhook_msg_"));
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn webhook_autosave_skips_group_reply_target() {
    let workspace = tempfile::TempDir::new().expect("test: autosave workspace");
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let sqlite = Arc::new(SqliteMemory::new(workspace.path()).expect("test: autosave sqlite"));
    let memory: Arc<dyn Memory> = sqlite.clone();
    let mut config = webhook_test_config();
    config.workspace_dir = workspace.path().to_path_buf();

    let mut state = AppState {
        config: crate::config::new_shared(config),
        provider,
        model: "test-model".into(),
        temperature: 0.0,
        mem: memory,
        auto_save: true,
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
    install_webhook_turn_runtime(&mut state);

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        HeaderMap::new(),
        Bytes::from_static(br#"{"message":"hello group","reply_target":"group:team-1"}"#),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        sqlite
            .list(Some(&MemoryCategory::Conversation), None)
            .await
            .expect("test: list autosaved messages")
            .is_empty()
    );
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn webhook_token_hash_is_deterministic_and_nonempty() {
    let secret_a = generate_test_secret();
    let secret_b = generate_test_secret();
    let one = hash_webhook_secret(&secret_a);
    let two = hash_webhook_secret(&secret_a);
    let other = hash_webhook_secret(&secret_b);

    assert_eq!(one, two);
    assert_ne!(one, other);
    assert_eq!(one.len(), 64);
}
