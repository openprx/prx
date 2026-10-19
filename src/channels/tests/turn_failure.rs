#![cfg(test)]
use super::*;

/// Wrap literal error text the way the runtime hands it to the classifier.
fn failure_of(text: &str) -> anyhow::Error {
    anyhow::anyhow!(text.to_string())
}

/// The 2026-08-21 production incident, verbatim from journald: a stale
/// image marker in replayed history made every turn in one chat fail for
/// hours while the user only ever read "Something went wrong".
#[test]
fn turn_failure_maps_unreadable_image_to_attachment_advice() {
    let err = failure_of(
        "failed to read local image '/home/ck/.openprx/workspace/media/img_9f31.jpg': No such file or directory (os error 2)",
    );
    assert_eq!(classify_turn_failure(&err), TurnFailureKind::Attachment);

    let notice = turn_failure_notice(TurnFailureKind::Attachment, "0badc0de");
    assert!(notice.contains("image or file"), "notice must name the attachment");
    assert!(
        notice.contains("send it again") && notice.contains("start a new conversation"),
        "notice must tell the reader what to do: {notice}"
    );
    assert!(!notice.contains("/home/ck"), "notice must not leak the path");
}

#[test]
fn turn_failure_maps_context_window_overflow() {
    for text in [
        "Anthropic API error (400 Bad Request): prompt is too long: 251234 tokens > 200000",
        "context_length_exceeded",
        "This model's maximum context length is 128000 tokens",
    ] {
        assert_eq!(
            classify_turn_failure(&failure_of(text)),
            TurnFailureKind::ContextTooLong,
            "expected an overflow classification for {text}"
        );
    }
    let notice = turn_failure_notice(TurnFailureKind::ContextTooLong, "0badc0de");
    assert!(notice.contains("too long"));
    assert!(notice.contains("start a new conversation") && notice.contains("shorter question"));
}

#[test]
fn turn_failure_maps_upstream_rate_limit() {
    // Both shapes seen in the logs: the streaming wrapper and a raw body.
    for text in [
        "Rate limited (HTTP 429): Kimi Code streaming HTTP 429 Too Many Requests: {\"error\":{\"message\":\"The engine is currently overloaded, please try again later\",\"type\":\"engine_overloaded_error\"}}",
        "Provider error: Anthropic streaming HTTP 429 Too Many Requests: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Error\"}}",
    ] {
        assert_eq!(
            classify_turn_failure(&failure_of(text)),
            TurnFailureKind::RateLimited,
            "expected a rate-limit classification for {text}"
        );
    }
    let notice = turn_failure_notice(TurnFailureKind::RateLimited, "0badc0de");
    assert!(
        notice.contains("wait a minute"),
        "notice must give a wait-and-retry action"
    );
}

#[test]
fn turn_failure_maps_billing_quota_exhaustion() {
    // Arrives on a 429 too, but retrying is futile, so it must not be told
    // to the user as "wait a minute".
    let err = failure_of(
        "GLM API error (429 Too Many Requests): {\"error\":{\"code\":1113,\"message\":\"insufficient balance\"}}",
    );
    assert_eq!(classify_turn_failure(&err), TurnFailureKind::QuotaExhausted);
    let notice = turn_failure_notice(TurnFailureKind::QuotaExhausted, "0badc0de");
    assert!(notice.contains("will not help") && notice.contains("administrator"));
    assert!(
        !notice.contains("wait a minute"),
        "a billing refusal must not advise waiting"
    );
}

#[test]
fn turn_failure_maps_credential_failure() {
    for text in [
        "Provider error: 401 Unauthorized: {\"error\":{\"message\":\"The API Key appears to be invalid or may have expired.\",\"type\":\"invalid_authentication_error\"}}",
        "OpenAI API key not set. Set OPENAI_API_KEY or edit config.toml.",
    ] {
        assert_eq!(
            classify_turn_failure(&failure_of(text)),
            TurnFailureKind::ServiceCredentials,
            "expected a credential classification for {text}"
        );
    }
    let notice = turn_failure_notice(TurnFailureKind::ServiceCredentials, "0badc0de");
    assert!(notice.contains("administrator") && notice.contains("error ID"));
    assert!(
        !notice.contains("OPENAI_API_KEY") && !notice.contains("config.toml"),
        "notice must not name internal configuration"
    );
}

#[test]
fn turn_failure_maps_upstream_server_failure() {
    for text in [
        "Kimi Code API error (503 Service Unavailable): upstream is down",
        "Provider error: Anthropic streaming HTTP 529: {\"type\":\"overloaded_error\"}",
    ] {
        assert_eq!(
            classify_turn_failure(&failure_of(text)),
            TurnFailureKind::ServiceUnavailable,
            "expected an unavailability classification for {text}"
        );
    }
    let notice = turn_failure_notice(TurnFailureKind::ServiceUnavailable, "0badc0de");
    assert!(notice.contains("try again in a few minutes"));
}

#[test]
fn turn_failure_maps_transport_failure() {
    for text in [
        "HTTP error: error sending request for url (https://llm-gw.internal.corp:8443/v1/messages)",
        "network retries exhausted (3): HTTP error: error decoding response body",
    ] {
        assert_eq!(
            classify_turn_failure(&failure_of(text)),
            TurnFailureKind::Network,
            "expected a transport classification for {text}"
        );
    }
    let notice = turn_failure_notice(TurnFailureKind::Network, "0badc0de");
    assert!(notice.contains("Please try again"));
    assert!(
        !notice.contains("internal.corp"),
        "notice must not leak the upstream host"
    );
}

#[test]
fn turn_failure_classification_walks_the_whole_cause_chain() {
    // Real failures reach this branch wrapped in generic context lines;
    // reading only the outermost message would classify them as unknown.
    let err = failure_of("failed to read local image '/srv/data/x.png': No such file or directory")
        .context("preparing messages for provider")
        .context("channel turn failed");
    assert_eq!(classify_turn_failure(&err), TurnFailureKind::Attachment);
}

/// SECURITY: the notice is assembled only from compile-time prose plus the
/// error id, so nothing an upstream puts in an error message can be carried
/// out to a chat. This asserts that end to end over the leak shapes that
/// really occur: absolute paths, API keys, internal hostnames, stacks and
/// SQL.
#[test]
fn turn_failure_notice_never_leaks_sensitive_error_details() {
    let secrets = [
        "/home/ck/.openprx/workspace/media/img_9f31.jpg",
        "sk-ant-api03-7Yq2Nn0ZzL8pVw4RtGh1JkMbXcAeDfSuIoPlQwErTyUi",
        "AKIAIOSFODNN7EXAMPLE",
        "llm-gw.internal.corp:8443",
        "postgres://prx:hunter2@10.0.3.14:5432/prx",
        "SELECT api_key FROM provider_credentials WHERE id = 7",
        "at openprx::providers::anthropic::stream (src/providers/anthropic.rs:1421)",
    ];
    let leaky = failure_of(&format!(
        "failed to read local image '{}': auth to {} with key {} failed; {}; {}; {}; {}",
        secrets[0], secrets[3], secrets[1], secrets[2], secrets[4], secrets[5], secrets[6]
    ));

    let kind = classify_turn_failure(&leaky);
    let notice = turn_failure_notice(kind, &turn_error_id("11112222-3333-4444-5555-666677778888"));

    for secret in secrets {
        assert!(
            !notice.contains(secret),
            "user-visible notice leaked {secret}: {notice}"
        );
    }
    // Structural guards, so a future edit that starts interpolating the
    // error is caught even for a payload not listed above.
    for fragment in ["/", "sk-", "AKIA", "SELECT ", "::", "src/", "@"] {
        assert!(
            !notice.contains(fragment),
            "user-visible notice contains raw-error marker {fragment:?}: {notice}"
        );
    }
    // Same guarantee for every class, not just the one this error mapped to.
    for kind in [
        TurnFailureKind::Attachment,
        TurnFailureKind::ContextTooLong,
        TurnFailureKind::RateLimited,
        TurnFailureKind::QuotaExhausted,
        TurnFailureKind::ServiceCredentials,
        TurnFailureKind::ServiceUnavailable,
        TurnFailureKind::Network,
        TurnFailureKind::Unclassified,
    ] {
        let text = turn_failure_notice(kind, "0badc0de");
        for fragment in ["/", "sk-", "SELECT ", "::", "src/", "@"] {
            assert!(
                !text.contains(fragment),
                "{} advice contains raw-error marker {fragment:?}: {text}",
                kind.log_label()
            );
        }
    }
}

/// Conservative fallback: an error nobody taught the classifier about still
/// gets the old wording — but now with a code an operator can grep for.
#[test]
fn unclassified_turn_failure_falls_back_to_generic_notice_with_error_id() {
    for text in [
        "Invalid SSE format: Invalid UTF-8: incomplete utf-8 byte sequence from index 1748",
        "Kimi Code API error (400 Bad Request): {\"error\":{\"message\":\"an assistant message with 'tool_calls' must be followed by tool messages\"}}",
        "wibble",
    ] {
        assert_eq!(
            classify_turn_failure(&failure_of(text)),
            TurnFailureKind::Unclassified,
            "unfamiliar error must not be guessed at: {text}"
        );
    }

    let notice = turn_failure_notice(TurnFailureKind::Unclassified, "0badc0de");
    assert!(notice.starts_with("⚠️ Something went wrong. Please try again later."));
    assert!(notice.contains("(Error ID: 0badc0de)"), "fallback still needs an id");
}

#[test]
fn turn_error_id_is_derived_from_the_turn_run_id() {
    let run_id = "3f9a1c02-8b4d-4e21-9f0a-1c2d3e4f5061";
    let id = turn_error_id(run_id);
    assert_eq!(id, "3f9a1c02", "the id must be the run id's leading hex digits");
    assert_eq!(id, turn_error_id(run_id), "the same turn must always yield the same id");
    assert_ne!(
        id,
        turn_error_id("a0000000-8b4d-4e21-9f0a-1c2d3e4f5061"),
        "different turns must not collide on the visible id"
    );

    // Total on a run id that is not UUID-shaped: hashed, still 8 hex digits.
    let hashed = turn_error_id("run/2026-08-22");
    assert_eq!(hashed.len(), 8);
    assert!(hashed.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(hashed, turn_error_id("run/2026-08-22"));
}

struct LeakyFailingProvider;

#[async_trait::async_trait]
impl Provider for LeakyFailingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!(
            "failed to read local image '/home/ck/.openprx/workspace/media/img_9f31.jpg': No such file or directory (os error 2)"
        ))
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!(
            "failed to read local image '/home/ck/.openprx/workspace/media/img_9f31.jpg': No such file or directory (os error 2)"
        ))
    }
}

/// End to end over a real channel turn: the reader gets actionable advice
/// with no path in it, and the code they can quote is the one derived from
/// this turn's run id — the same value the failure is logged under.
#[tokio::test]
async fn failed_channel_turn_sends_classified_advice_carrying_the_turn_error_id() {
    let tmp = TempDir::new().expect("test: tempdir");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let mut runtime_config = Config::default();
    runtime_config.default_provider = Some("test-provider".to_string());
    runtime_config.default_model = Some("test-model".to_string());
    let config_manager = crate::config::new_shared(runtime_config);
    let ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: Arc::clone(&config_manager),
        config_generation: config_manager.pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(LeakyFailingProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::clone(&memory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(HookManager::new(tmp.path().to_path_buf())),
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
        workspace_dir: Arc::new(tmp.path().to_path_buf()),

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

    run_owned_channel_probe(ctx, gate_test_message(), CancellationToken::new()).await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    let responses = response_messages(&sent_messages);
    assert_eq!(responses.len(), 1, "a failed turn must still answer once");
    let response = responses.first().expect("test: one response");

    assert!(
        response.contains(TurnFailureKind::Attachment.advice()),
        "the reader must be told the attachment is the problem: {response}"
    );
    assert!(
        !response.contains("/home/ck") && !response.contains("img_9f31") && !response.contains("os error"),
        "the outbound message leaked internal error detail: {response}"
    );

    // The user-visible code must resolve to this turn's run id, which is
    // what `tracing` records the failure under and what the durable
    // terminal event is keyed on.
    let events = memory
        .list_message_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: tmp.path().to_string_lossy().to_string(),
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: Some("test-channel".to_string()),
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            20,
        )
        .await
        .expect("test: list message events");
    let run_id = events
        .iter()
        .find(|event| event.role == "user")
        .and_then(|event| event.run_id.clone())
        .expect("test: the inbound event carries the turn run id");
    assert!(
        response.contains(&format!("(Error ID: {})", turn_error_id(&run_id))),
        "the quoted code must match the id logged for run {run_id}: {response}"
    );
}
