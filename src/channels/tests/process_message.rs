#![cfg(test)]
use super::*;

fn tool_call_payload_with_alias_tag() -> String {
    r#"<toolcall>
{"name":"mock_price","arguments":{"symbol":"BTC"}}
</toolcall>"#
        .to_string()
}

struct ToolCallingAliasProvider;

#[async_trait::async_trait]
impl Provider for ToolCallingAliasProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(tool_call_payload_with_alias_tag())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let has_tool_results = messages
            .iter()
            .any(|msg| msg.role == "user" && msg.content.contains("[Tool results]"));
        if has_tool_results {
            Ok("BTC alias-tag flow resolved to final text output.".to_string())
        } else {
            Ok(tool_call_payload_with_alias_tag())
        }
    }
}

struct RawToolArtifactProvider;

#[async_trait::async_trait]
impl Provider for RawToolArtifactProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("fallback".to_string())
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(r#"{"name":"mock_price","parameters":{"symbol":"BTC"}}
{"result":{"symbol":"BTC","price_usd":65000}}
BTC is currently around $65,000 based on latest tool output."#
            .to_string())
    }
}

struct IterativeToolProvider {
    required_tool_iterations: usize,
}

impl IterativeToolProvider {
    fn completed_tool_iterations(messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .filter(|msg| msg.role == "user" && msg.content.contains("[Tool results]"))
            .count()
    }
}

#[async_trait::async_trait]
impl Provider for IterativeToolProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(tool_call_payload())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let completed_iterations = Self::completed_tool_iterations(messages);
        if completed_iterations >= self.required_tool_iterations {
            Ok(format!("Completed after {completed_iterations} tool iterations."))
        } else {
            Ok(tool_call_payload())
        }
    }
}

#[tokio::test]
async fn process_channel_message_strips_unexecuted_tool_json_artifacts_from_reply() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(RawToolArtifactProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![Box::new(MockPriceTool::default())]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(workspace.path().to_path_buf())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(workspace.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig {
            always_include: vec!["mock_price".to_string()],
            ..crate::config::ToolTieringConfig::default()
        },
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        runtime_ctx,
        traits::ChannelMessage {
            id: "msg-raw-json".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-raw".to_string(),
            content: "What is the BTC price now?".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 3,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    let responses = response_messages(&sent_messages);
    assert_eq!(responses.len(), 1);
    let response = responses.first().unwrap();
    assert!(response.starts_with("chat-raw:"));
    assert!(response.contains("BTC is currently around"));
    assert!(!response.contains("\"name\":\"mock_price\""));
    assert!(!response.contains("\"result\""));
}

#[tokio::test]
async fn process_channel_message_executes_tool_calls_with_alias_tags() {
    let dir = TempDir::new().expect("test: tempdir");
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    // A refusal reaches the model as a [Tool results] block too, so count
    // the executions rather than trusting the canned reply.
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(ToolCallingAliasProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![Box::new(MockPriceTool {
            executions: Arc::clone(&executions),
        })]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(dir.path().to_path_buf())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(dir.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig {
            always_include: vec!["mock_price".to_string()],
            ..crate::config::ToolTieringConfig::default()
        },
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        runtime_ctx,
        traits::ChannelMessage {
            id: "msg-2".to_string(),
            sender: "bob".to_string(),
            reply_target: "chat-84".to_string(),
            content: "What is the BTC price now?".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 2,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    let responses = response_messages(&sent_messages);
    assert_eq!(responses.len(), 1);
    let response = responses.first().unwrap();
    assert!(response.starts_with("chat-84:"));
    assert!(response.contains("alias-tag flow resolved"));
    assert!(!response.contains("<toolcall>"));
    assert!(!response.contains("mock_price"));
    drop(sent_messages);
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the alias-tagged call must actually have executed the tool, not been refused"
    );
}

#[test]
fn signal_vision_report_requires_confident_explicit_ok_and_summary() {
    assert!(signal_vision_report_context(r#"{"status":"ok","confidence":0.60,"summary":"visible object"}"#).is_some());
    for raw in [
        r#"{"status":"uncertain","confidence":1.0,"summary":"visible object"}"#,
        r#"{"status":"unexpected","confidence":1.0,"summary":"visible object"}"#,
        r#"{"status":"ok","confidence":0.59,"summary":"visible object"}"#,
        r#"{"status":"ok","confidence":1.01,"summary":"visible object"}"#,
        r#"{"status":"ok","summary":"visible object"}"#,
        r#"{"status":"ok","confidence":0.8,"summary":" "}"#,
        r#"{"status":"ok","confidence":NaN,"summary":"visible object"}"#,
    ] {
        assert!(
            signal_vision_report_context(raw).is_none(),
            "invalid report must fall back"
        );
    }
}

#[test]
fn signal_vision_report_retains_fenced_json_and_observation_compatibility() {
    let context = signal_vision_report_context(
        "```json\n{\"status\":\"ok\",\"confidence\":0.9,\"observation\":\"visible object\"}\n```",
    )
    .expect("valid fenced observation report");
    assert!(context.contains("visible object"));
}

#[tokio::test]
async fn process_channel_message_signal_image_without_vision_result_uses_uncertainty_fallback() {
    // The real metered vision transport returns an empty report. The shared
    // fixture keeps one physical send, fallback/redaction and owned cleanup assertions.
    signal_accounted::exercise_empty_report().await;
}

#[tokio::test]
async fn process_channel_message_signal_text_only_path_is_unchanged() {
    signal_accounted::exercise_text().await;
}

#[tokio::test]
async fn process_channel_message_handles_models_command_without_llm_call() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let default_provider_impl = Arc::new(ModelCaptureProvider::default());
    let default_provider: Arc<dyn Provider> = default_provider_impl.clone();
    let fallback_provider_impl = Arc::new(ModelCaptureProvider::default());
    let fallback_provider: Arc<dyn Provider> = fallback_provider_impl.clone();

    let mut effective = Config::default();
    effective.default_provider = Some("test-provider".to_string());
    effective.default_model = Some("default-model".to_string());
    let config_manager = crate::config::new_shared(effective);
    let config_generation = config_manager.pin();
    let mut provider_cache_seed: HashMap<ProviderCacheKey, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert(
        (
            config_generation.id.0,
            "test-provider".to_string(),
            "default-model".to_string(),
        ),
        Arc::clone(&default_provider),
    );
    provider_cache_seed.insert(
        (
            config_generation.id.0,
            "openrouter".to_string(),
            "default-model".to_string(),
        ),
        fallback_provider,
    );

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: config_manager,
        config_generation,
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::clone(&default_provider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(workspace.path().to_path_buf())),
        model: Arc::new("default-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(workspace.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        runtime_ctx.clone(),
        traits::ChannelMessage {
            id: "msg-cmd-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "/models openrouter".to_string(),
            channel: "telegram".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    let sent = channel_impl.sent_messages.lock().await;
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("Provider switched to `openrouter`"));

    // The route override is written under the canonical key (FIX-P1-25b);
    // the legacy key is left untouched (read-merge writes canonical only), so
    // it must not appear here since this session never had a legacy override.
    let routes = runtime_ctx.route_overrides.lock();
    assert!(
        routes.get("telegram_alice").is_none(),
        "writes go to the canonical key only; no legacy route key is created"
    );
    let route = routes
        .get("channel:telegram:alice:chat-1")
        .cloned()
        .expect("route should be stored for sender under canonical key");
    drop(routes);
    assert_eq!(route.provider, "openrouter");
    assert_eq!(route.model, "default-model");

    assert_eq!(default_provider_impl.call_count.load(Ordering::SeqCst), 0);
    assert_eq!(fallback_provider_impl.call_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn process_channel_message_uses_route_override_provider_and_model() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let default_provider_impl = Arc::new(ModelCaptureProvider::default());
    let default_provider: Arc<dyn Provider> = default_provider_impl.clone();
    let routed_provider_impl = Arc::new(ModelCaptureProvider::default());
    let routed_provider: Arc<dyn Provider> = routed_provider_impl.clone();

    let config_manager = crate::config::new_shared(Config::default());
    let config_generation = config_manager.pin();
    let mut provider_cache_seed: HashMap<ProviderCacheKey, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert(
        (
            config_generation.id.0,
            "test-provider".to_string(),
            "default-model".to_string(),
        ),
        Arc::clone(&default_provider),
    );
    provider_cache_seed.insert(
        (
            config_generation.id.0,
            "openrouter".to_string(),
            "route-model".to_string(),
        ),
        routed_provider,
    );

    let route_key = "telegram_alice".to_string();
    let mut route_overrides = HashMap::new();
    route_overrides.insert(
        route_key,
        ChannelRouteSelection {
            provider: "openrouter".to_string(),
            model: "route-model".to_string(),
        },
    );

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: config_manager,
        config_generation,
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::clone(&default_provider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(workspace.path().to_path_buf())),
        model: Arc::new("default-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(route_overrides)),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(workspace.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        runtime_ctx,
        traits::ChannelMessage {
            id: "msg-routed-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "hello routed provider".to_string(),
            channel: "telegram".to_string(),
            timestamp: 2,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    assert_eq!(default_provider_impl.call_count.load(Ordering::SeqCst), 0);
    assert_eq!(routed_provider_impl.call_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        routed_provider_impl.models.lock().as_slice(),
        &["route-model".to_string()]
    );
}

#[tokio::test]
async fn process_channel_message_builds_selected_provider_without_plain_turn_retrieval() {
    factory_reload_fixture::run().await;
}

#[tokio::test]
async fn process_channel_message_uses_runtime_default_model_from_store() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(ModelCaptureProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let temp = tempfile::TempDir::new().expect("temp dir");
    let mut live_config = Config::default();
    live_config.default_provider = Some("test-provider".to_string());
    live_config.default_model = Some("hot-reloaded-model".to_string());
    live_config.default_temperature = 0.5;
    let config_manager = crate::config::new_shared(live_config);
    let config_generation = config_manager.pin();
    let mut provider_cache_seed: HashMap<ProviderCacheKey, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert(
        (
            config_generation.id.0,
            "test-provider".to_string(),
            "hot-reloaded-model".to_string(),
        ),
        Arc::clone(&provider),
    );

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: config_manager,
        config_generation,
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::clone(&provider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(workspace.path().to_path_buf())),
        model: Arc::new("startup-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions {
            openprx_dir: Some(temp.path().to_path_buf()),
            ..providers::ProviderRuntimeOptions::default()
        },
        workspace_dir: Arc::new(workspace.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        runtime_ctx,
        traits::ChannelMessage {
            id: "msg-runtime-store-model".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "hello runtime defaults".to_string(),
            channel: "telegram".to_string(),
            timestamp: 4,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    assert_eq!(provider_impl.call_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider_impl.models.lock().as_slice(),
        &["hot-reloaded-model".to_string()]
    );
}

#[tokio::test]
async fn process_channel_message_allows_long_tool_sequence_to_finish() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(IterativeToolProvider {
            required_tool_iterations: 11,
        }),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![Box::new(MockPriceTool::default())]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(workspace.path().to_path_buf())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(workspace.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig {
            always_include: vec!["mock_price".to_string()],
            ..crate::config::ToolTieringConfig::default()
        },
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        runtime_ctx,
        traits::ChannelMessage {
            id: "msg-iter-success".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-iter-success".to_string(),
            content: "Loop until done".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    let responses = response_messages(&sent_messages);
    assert_eq!(responses.len(), 1);
    let response = responses.first().unwrap();
    assert!(response.starts_with("chat-iter-success:"));
    assert!(response.contains("Completed after 11 tool iterations."));
    assert!(!response.contains("⚠️ Error:"));
}

// --- User-facing failure classification -------------------------------
//
// The sentence a chat user reads when a turn fails used to be one
// zero-information line for every cause. These tests pin the two things
// that matter: the advice is right for the class, and no error text ever
// reaches the chat.
