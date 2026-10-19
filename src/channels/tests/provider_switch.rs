#![cfg(test)]
use super::*;

struct ProviderReloadPermit;

struct PreparedProviderReloadPermit;

impl crate::config::ConfigGenerationParticipant for ProviderReloadPermit {
    fn name(&self) -> &'static str {
        "channel-provider-reload-proof"
    }

    fn supports_rebuild_field(&self, field: &str) -> bool {
        matches!(field, "api_key" | "api_url")
    }

    fn prepare(
        &self,
        _generation: Arc<crate::config::ConfigGeneration>,
        _changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn crate::config::PreparedConfigGeneration>> {
        Ok(Box::new(PreparedProviderReloadPermit))
    }
}

impl crate::config::PreparedConfigGeneration for PreparedProviderReloadPermit {
    fn commit(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn rollback(&mut self) {}
}

/// A credential-shaped sentinel configured for a different provider.
const ROOT_CONNECTION_SENTINEL: &str = "root-secret";
/// A credential-shaped sentinel belonging to the BYO endpoint itself.
const BYO_CONNECTION_SENTINEL: &str = "byo-secret";

/// A loopback endpoint owned by the test. It needs no authentication,
/// answers every request with an empty JSON body, and records the headers
/// it was sent so an assertion can name a credential that reached the wire.
struct ConnectionRecordingEndpoint {
    addr: std::net::SocketAddr,
    recorded: Arc<Mutex<Vec<Vec<(String, String)>>>>,
    server: tokio::task::JoinHandle<()>,
}

impl ConnectionRecordingEndpoint {
    fn provider_name(&self) -> String {
        format!("custom:http://{}/v1", self.addr)
    }

    fn requests(&self) -> Vec<Vec<(String, String)>> {
        self.recorded.lock().clone()
    }

    fn carried(&self, sentinel: &str) -> bool {
        self.requests()
            .iter()
            .any(|headers| headers.iter().any(|(_, value)| value.contains(sentinel)))
    }

    fn shutdown(self) {
        self.server.abort();
    }
}

async fn spawn_connection_recording_endpoint() -> ConnectionRecordingEndpoint {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: loopback address");
    let recorded: Arc<Mutex<Vec<Vec<(String, String)>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&recorded);

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _peer)) = listener.accept().await else {
                return;
            };
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                let mut buffer: Vec<u8> = Vec::with_capacity(4096);
                let mut chunk = [0_u8; 2048];
                let head_end = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break position + 4;
                    }
                    if buffer.len() > 262_144 {
                        return;
                    }
                };

                let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                let headers: Vec<(String, String)> = head
                    .split("\r\n")
                    .skip(1)
                    .take_while(|line| !line.is_empty())
                    .filter_map(|line| {
                        line.split_once(':')
                            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
                    })
                    .collect();
                sink.lock().push(headers);

                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await;
                let _ = socket.flush().await;
            });
        }
    });

    ConnectionRecordingEndpoint { addr, recorded, server }
}

/// Runtime options rooted in a temp directory, so credential resolution
/// reads an empty auth-profile store instead of the operator's own.
fn isolated_provider_runtime_options(temp: &TempDir) -> providers::ProviderRuntimeOptions {
    providers::ProviderRuntimeOptions {
        openprx_dir: Some(temp.path().to_path_buf()),
        secrets_encrypt: false,
        codex_auth_json_path: Some(temp.path().join("absent-codex-auth.json")),
        codex_auth_json_auto_import: false,
        ..providers::ProviderRuntimeOptions::default()
    }
}

fn channel_reliability_without_fallbacks() -> crate::config::ReliabilityConfig {
    crate::config::ReliabilityConfig {
        provider_retries: 0,
        provider_backoff_ms: 0,
        fallback_providers: Vec::new(),
        api_keys: Vec::new(),
        ..crate::config::ReliabilityConfig::default()
    }
}

/// PMM P0-A. A `/models <provider>` switch reaches
/// `get_or_create_provider_for_generation` with a provider that is not the
/// configured default. The root credential belongs to that default, so it
/// must not be presented to the selection — not in the constructed
/// provider, and not on the wire during warmup.
///
/// RED before the resolver: the endpoint receives the root sentinel in an
/// `Authorization` header.
#[tokio::test]
async fn a_provider_switch_does_not_release_the_root_connection() {
    let endpoint = spawn_connection_recording_endpoint().await;
    let temp = TempDir::new().expect("test: temp state dir");
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.default_provider = Arc::new("openai".to_string());
    ctx.api_key = Some(ROOT_CONNECTION_SENTINEL.to_string());
    ctx.api_url = Some("https://api.openai.com/v1".to_string());
    ctx.reliability = Arc::new(channel_reliability_without_fallbacks());
    ctx.provider_runtime_options = isolated_provider_runtime_options(&temp);
    let generation = Arc::clone(&ctx.config_generation);

    let outcome = get_or_create_provider_for_generation(
        &ctx,
        &endpoint.provider_name(),
        providers::selection::default_model(&generation.effective),
        &generation,
    )
    .await;

    assert!(
        outcome.is_ok(),
        "a keyless BYO endpoint is usable on its own: {:?}",
        outcome.err().map(|error| format!("{error:#}"))
    );
    assert!(
        !endpoint.carried(ROOT_CONNECTION_SENTINEL),
        "the root credential must not reach another provider's endpoint: {:?}",
        endpoint.requests()
    );
    assert_eq!(
        endpoint.requests().len(),
        0,
        "without a credential of its own there is nothing to warm up"
    );
    endpoint.shutdown();
}

/// P0-A residual: the rotation keys in `reliability.api_keys` are extra
/// credentials for the root key, so a `/models` switch to another provider
/// must not carry them either. The endpoint throttles its first request,
/// which is exactly when a chain rotates to its next keyed member.
///
/// RED before the fix: a rotated member presents the rotation sentinel.
#[tokio::test]
async fn a_provider_switch_does_not_release_the_rotation_keys() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
    const ROTATION_SENTINEL: &str = "C0-CHANNEL-ROTATION-SENTINEL";
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let temp = TempDir::new().expect("test: temp state dir");
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.default_provider = Arc::new("openai".to_string());
    ctx.api_key = Some(ROOT_CONNECTION_SENTINEL.to_string());
    ctx.api_url = Some("https://api.openai.com/v1".to_string());
    ctx.reliability = Arc::new(crate::config::ReliabilityConfig {
        api_keys: vec![ROTATION_SENTINEL.to_string()],
        ..channel_reliability_without_fallbacks()
    });
    ctx.provider_runtime_options = isolated_provider_runtime_options(&temp);
    let generation = Arc::clone(&ctx.config_generation);

    let provider = get_or_create_provider_for_generation(&ctx, &probe.custom_provider(), PROBE_MODEL, &generation)
        .await
        .expect("test: a keyless BYO endpoint is usable on its own");
    let _reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

    assert!(
        !probe.requests().is_empty(),
        "the switched provider must reach the endpoint"
    );
    assert!(
        !probe.saw(ROTATION_SENTINEL),
        "a root rotation key reached another provider's endpoint: {:?}",
        probe.requests()
    );
    assert!(!probe.saw(ROOT_CONNECTION_SENTINEL));
}

/// The same switch, when the selection *is* the configured default under a
/// different spelling, still receives its own connection: the resolver
/// compares provider identity, not bytes.
///
/// Two spellings are exercised, and neither is the byte-identical one the
/// production short-circuit answers before the resolver is reached. The
/// trailing-slash spelling does not depend on the prefix case folding, so
/// this positive coverage survives a revert of that folding (R-C1 F-10).
#[tokio::test]
async fn a_provider_switch_keeps_the_connection_of_the_configured_default() {
    let endpoint = spawn_connection_recording_endpoint().await;
    let temp = TempDir::new().expect("test: temp state dir");
    let configured = endpoint.provider_name();
    let trailing_slash_spelling = format!("{configured}/");
    let selected = format!("CUSTOM:{}", configured.trim_start_matches("custom:"));
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.default_provider = Arc::new(configured.clone());
    ctx.api_key = Some(BYO_CONNECTION_SENTINEL.to_string());
    ctx.api_url = None;
    ctx.reliability = Arc::new(channel_reliability_without_fallbacks());
    ctx.provider_runtime_options = isolated_provider_runtime_options(&temp);
    let mut effective = Config::default();
    effective.workspace_dir = temp.path().to_path_buf();
    effective.default_provider = Some(configured);
    effective.api_key = Some(BYO_CONNECTION_SENTINEL.to_string());
    effective.reliability = channel_reliability_without_fallbacks();
    let manager = crate::config::new_shared(effective);
    ctx.config = Arc::clone(&manager);
    ctx.config_generation = manager.pin();
    let generation = Arc::clone(&ctx.config_generation);

    let outcome = get_or_create_provider_for_generation(
        &ctx,
        &selected,
        providers::selection::default_model(&generation.effective),
        &generation,
    )
    .await;

    assert!(
        outcome.is_ok(),
        "the configured endpoint must still build: {:?}",
        outcome.err().map(|error| format!("{error:#}"))
    );
    assert!(
        endpoint.carried(BYO_CONNECTION_SENTINEL),
        "the endpoint's own credential must still be presented: {:?}",
        endpoint.requests()
    );

    // The first spelling already put the sentinel on the wire, so the
    // second is proven only by a request of its own that carries it
    // (SP-INT2 a29): `carried` alone stays true whatever this call sends.
    let requests_before_trailing_slash = endpoint.requests().len();
    let trailing_slash = get_or_create_provider_for_generation(
        &ctx,
        &trailing_slash_spelling,
        providers::selection::default_model(&generation.effective),
        &generation,
    )
    .await;
    assert!(
        trailing_slash.is_ok(),
        "one endpoint spelled with a trailing slash is the same endpoint: {:?}",
        trailing_slash.err().map(|error| format!("{error:#}"))
    );
    let requests = endpoint.requests();
    assert!(
        requests.len() > requests_before_trailing_slash,
        "the trailing-slash spelling must reach the endpoint itself: {requests:?}"
    );
    assert!(
        requests
            .last()
            .is_some_and(|headers| headers.iter().any(|(_, value)| value.contains(BYO_CONNECTION_SENTINEL))),
        "the trailing-slash spelling must present the endpoint's own credential: {requests:?}"
    );
    endpoint.shutdown();
}

#[test]
fn channel_message_admission_pins_the_current_manager_generation() {
    let manager = crate::config::new_shared(Config::default());
    let initial_generation = manager.pin();
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = Arc::clone(&manager);
    ctx.config_generation = Arc::clone(&initial_generation);

    let mut desired = (*initial_generation.effective).clone();
    desired.default_temperature = 0.42;
    desired.agent.read_only_tool_concurrency_window = 7;
    manager
        .apply_runtime_config(desired, crate::config::ConfigReloadTrigger::Test)
        .expect("snapshot-hot channel fields must publish");

    let current_generation = manager.pin();
    ctx.config_generation = Arc::clone(&current_generation);
    let current_defaults = runtime_defaults_for_generation(&ctx, &current_generation);
    let current_message = message_runtime_snapshot(&ctx, &current_generation);
    let pinned_before_reload = message_runtime_snapshot(&ctx, &initial_generation);

    assert_ne!(current_generation.id, initial_generation.id);
    assert_eq!(current_defaults.temperature, 0.42);
    assert_eq!(current_message.read_only_tool_concurrency_window, 7);
    assert_ne!(
        current_message.read_only_tool_concurrency_window,
        pinned_before_reload.read_only_tool_concurrency_window
    );
}

/// The channel turn's tiering policy is the *channel* policy, not the raw
/// config: the snapshot is what `process_channel_message` hands to the tool
/// loop and to `core_dependency_is_available`, so the fold has to happen
/// here rather than being remembered at each call site.
///
/// MUTATION GUARD: change `message_runtime_snapshot` back to
/// `config.tool_tiering.clone()` and this test goes red.
#[test]
fn channel_message_runtime_hides_the_operations_tools_by_default() {
    let manager = crate::config::new_shared(Config::default());
    let generation = manager.pin();
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = Arc::clone(&manager);
    ctx.config_generation = Arc::clone(&generation);
    ctx.tool_tiering = generation.effective.tool_tiering.clone();

    let snapshot = message_runtime_snapshot(&ctx, &generation);
    assert!(
        generation.effective.tool_tiering.always_exclude.is_empty(),
        "fixture must start from an operator config that excludes nothing"
    );
    for hidden in crate::config::DEFAULT_CHANNEL_EXCLUDED_TOOLS {
        assert!(
            snapshot
                .tool_tiering
                .always_exclude
                .iter()
                .any(|excluded| excluded == hidden),
            "`{hidden}` must be excluded on a channel turn: {:?}",
            snapshot.tool_tiering.always_exclude
        );
    }
}

// ── R5 / OQ-23: an IM turn budgets on the served model's effective window ──

/// The command/classifier getter retains cached-provider preference.
/// Admitted message turns use their separately validated factory path.
#[tokio::test]
async fn generation_provider_getter_prefers_cached_default_instance() {
    let startup_provider_impl = Arc::new(ModelCaptureProvider::default());
    let startup_provider: Arc<dyn Provider> = startup_provider_impl.clone();
    let reloaded_provider_impl = Arc::new(ModelCaptureProvider::default());
    let reloaded_provider: Arc<dyn Provider> = reloaded_provider_impl.clone();
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.provider = startup_provider;
    ctx.provider_cache.lock().insert(
        (
            ctx.config_generation.id.0,
            "test-provider".to_string(),
            "default-model".to_string(),
        ),
        Arc::clone(&reloaded_provider),
    );

    let selected =
        get_or_create_provider_for_generation(&ctx, "test-provider", "default-model", &ctx.config_generation)
            .await
            .expect("cached generation provider");
    assert!(Arc::ptr_eq(&selected, &reloaded_provider));
    let response = selected
        .chat_with_history(
            &[ChatMessage::user("hello cached default provider")],
            "default-model",
            0.0,
        )
        .await
        .expect("cached fixture response");

    assert_eq!(response, "ok");
    assert_eq!(startup_provider_impl.call_count.load(Ordering::SeqCst), 0);
    assert_eq!(reloaded_provider_impl.call_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        reloaded_provider_impl.models.lock().as_slice(),
        &["default-model".to_string()]
    );
}

#[tokio::test]
async fn generation_provider_cache_separates_models_on_the_same_provider() {
    let ctx = ctx_with_histories(HashMap::new());
    let selected_provider = "custom:http://127.0.0.1:9/v1";

    let model_a = get_or_create_provider_for_generation(&ctx, selected_provider, "model-a", &ctx.config_generation)
        .await
        .expect("first model builds");
    let model_a_again =
        get_or_create_provider_for_generation(&ctx, selected_provider, "model-a", &ctx.config_generation)
            .await
            .expect("first model is cached");
    let model_b = get_or_create_provider_for_generation(&ctx, selected_provider, "model-b", &ctx.config_generation)
        .await
        .expect("second model builds its own chain");

    assert!(Arc::ptr_eq(&model_a, &model_a_again), "the exact cache key was rebuilt");
    assert!(
        !Arc::ptr_eq(&model_a, &model_b),
        "a provider chain resolved for model-a was reused for model-b"
    );
    assert_eq!(ctx.provider_cache.lock().len(), 2);
}

#[tokio::test]
async fn generation_provider_cache_rebuilds_the_same_default_after_reload() {
    const OLD_KEY: &str = "prx-channel-cache-key-old";
    const NEW_KEY: &str = "prx-channel-cache-key-new";

    let old_endpoint = providers::credential_probe::CredentialProbe::start().await;
    let new_endpoint = providers::credential_probe::CredentialProbe::start().await;
    let temp = TempDir::new().expect("test: temp state dir");
    let mut initial = Config {
        workspace_dir: temp.path().to_path_buf(),
        ..Config::default()
    };
    initial.default_provider = Some("openai".to_string());
    initial.default_model = Some("gpt-4o".to_string());
    initial.api_key = Some(OLD_KEY.to_string());
    initial.api_url = Some(old_endpoint.base_url());
    let manager = crate::config::new_shared(initial);
    let old_generation = manager.pin();

    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = Arc::clone(&manager);
    ctx.config_generation = Arc::clone(&old_generation);
    ctx.default_provider = Arc::new("openai".to_string());
    ctx.provider_runtime_options = isolated_provider_runtime_options(&temp);
    let stale: Arc<dyn Provider> = Arc::new(ModelCaptureProvider::default());
    let stale_weak = Arc::downgrade(&stale);
    ctx.provider_cache
        .lock()
        .insert((old_generation.id.0, "openai".to_string(), "gpt-4o".to_string()), stale);

    let participant: Arc<dyn crate::config::ConfigGenerationParticipant> = Arc::new(ProviderReloadPermit);
    manager.register_participant(&participant);
    let mut reloaded = old_generation.effective.as_ref().clone();
    reloaded.api_key = Some(NEW_KEY.to_string());
    reloaded.api_url = Some(new_endpoint.base_url());
    let report = manager
        .apply_runtime_config(reloaded, crate::config::ConfigReloadTrigger::Test)
        .expect("same-route credential reload");
    assert_eq!(report.rebuilt, vec!["api_key", "api_url"]);
    let new_generation = manager.pin();
    assert_ne!(new_generation.id, old_generation.id);
    ctx.config_generation = Arc::clone(&new_generation);

    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let command = traits::ChannelMessage {
        sender: "prx_user".to_string(),
        reply_target: "prx_channel".to_string(),
        channel: "telegram".to_string(),
        content: "/models openai".to_string(),
        ..traits::ChannelMessage::default()
    };
    assert!(
        handle_runtime_command_if_needed(&ctx, &new_generation, &command, Some(&channel)).await,
        "the production /models command did not claim its input"
    );
    assert!(
        channel_impl
            .sent_messages
            .lock()
            .await
            .iter()
            .any(|message| message.contains("Provider switched to `openai`")),
        "the production /models command did not report readiness"
    );

    let selected = get_or_create_provider_for_generation(&ctx, "openai", "gpt-4o", &new_generation)
        .await
        .expect("cached reloaded default provider");
    let response = selected
        .simple_chat("verify reloaded channel provider", "gpt-4o", 0.0)
        .await
        .expect("reloaded provider response");

    assert_eq!(response, providers::credential_probe::PROBE_REPLY);
    assert!(old_endpoint.requests().is_empty(), "the startup endpoint was reused");
    let requests = new_endpoint.requests();
    assert!(!requests.is_empty(), "the reloaded endpoint was not called");
    assert!(
        requests
            .iter()
            .all(|request| request.carries(NEW_KEY) && !request.carries(OLD_KEY)),
        "the reloaded provider did not keep the new credential owner: {requests:?}"
    );
    assert!(
        stale_weak.upgrade().is_none(),
        "the stale credential chain remained cached"
    );
    let cached = ctx.provider_cache.lock();
    assert_eq!(cached.len(), 1);
    assert!(cached.contains_key(&(new_generation.id.0, "openai".to_string(), "gpt-4o".to_string())));
}

/// Finding 64: the per-sender `/models` provider getter resolves the
/// selected model's fallback contract before caching or warming a chain.
#[tokio::test]
async fn per_sender_provider_getter_refuses_a_cross_dialect_fallback_before_transport() {
    let temp = TempDir::new().expect("test: temp state dir");
    let probe = providers::credential_probe::CredentialProbe::start().await;
    let selected_provider = probe.custom_provider();
    let mut effective = Config {
        workspace_dir: temp.path().to_path_buf(),
        ..Config::default()
    };
    crate::providers::entrypoint_consistency_tests::configure_cross_dialect(&mut effective, &selected_provider);
    effective.default_provider = Some("openai".to_string());
    let manager = crate::config::new_shared(effective);
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.default_provider = Arc::new("openai".to_string());
    ctx.provider_runtime_options = isolated_provider_runtime_options(&temp);
    ctx.config = Arc::clone(&manager);
    ctx.config_generation = manager.pin();

    let error = get_or_create_provider_for_generation(
        &ctx,
        &selected_provider,
        crate::providers::entrypoint_consistency_tests::PRIMARY_MODEL,
        &ctx.config_generation,
    )
    .await
    .err()
    .expect("the per-sender getter accepted an incompatible fallback");
    let shown = format!("{error:#}");
    crate::providers::entrypoint_consistency_tests::assert_cross_dialect_refusal("channel per-sender", &shown);
    assert!(
        probe.requests().is_empty(),
        "the channel getter reached transport: {shown}"
    );
    assert!(
        ctx.provider_cache.lock().is_empty(),
        "a refused chain entered the cache"
    );
}

/// R-1: `/models` shows the sender's current provider to the IM chat,
/// and the default route is the configured label, which may carry
/// `user:password@`.
#[test]
fn the_providers_help_names_the_current_endpoint_without_url_userinfo() {
    let current = ChannelRouteSelection {
        provider: "custom:http://alice:SECRET-fixture-pw@127.0.0.1:1/v1".to_string(),
        model: "fixture-model".to_string(),
    };
    let reply = build_providers_help_response(&current);
    assert!(
        reply.contains("custom:http://[redacted]@127.0.0.1:1/v1"),
        "the endpoint must stay recognisable: {reply}"
    );
    assert!(!reply.contains("SECRET-fixture-pw"), "{reply}");
}

/// C2b-2: a tool-schema refusal is a configuration problem. Retrying the
/// message cannot help, so the IM user must not be told to try again
/// later; the notice says an administrator has to act and gives the
/// error id. The tool list stays in the log, not in the chat.
#[tokio::test]
async fn a_tool_schema_refusal_is_reported_as_a_configuration_problem() {
    let error = crate::providers::tool_schema_refusal_tests::refusal_through_a_chain().await;
    let kind = classify_turn_failure(&error);
    assert_ne!(kind, TurnFailureKind::Unclassified, "{error:#}");
    let notice = turn_failure_notice(kind, "0badc0de");
    assert!(notice.contains("0badc0de"), "{notice}");
    assert!(
        notice.contains("configur"),
        "the notice must name a configuration problem: {notice}"
    );
    assert!(
        !notice.to_lowercase().contains("try again"),
        "retrying cannot help: {notice}"
    );
    assert!(
        !notice.contains("fixture_alternatives"),
        "tool names stay out of the chat: {notice}"
    );
}

// ── PMM-C: channel selections go through the resolver ─────────────

fn decision_only_config(provider: &str) -> Config {
    let mut config = Config::default();
    let temp = std::env::temp_dir().join(format!("pmmc-channels-{}", std::process::id()));
    config.workspace_dir = temp.clone();
    config.config_path = temp.join("config.toml");
    config.default_provider = Some(provider.to_string());
    config.default_model = Some("jev-decision-test".to_string());
    config.api_key = Some("PMMC-CHANNEL-ROOT".to_string());
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"{provider}\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n"
        ))
        .expect("test: declaration"),
    );
    config
}

#[tokio::test]
async fn a_model_switch_to_a_decision_only_model_is_refused_and_the_route_kept() {
    let mut config = decision_only_config("openrouter");
    config.default_model = Some("vendor/chat-model".to_string());
    let shared = crate::config::new_shared(config);
    let generation = shared.pin();
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = shared.clone();
    ctx.config_generation = Arc::clone(&generation);
    ctx.default_provider = Arc::new("openrouter".to_string());
    let msg = traits::ChannelMessage {
        id: "msg-model-1".to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-1".to_string(),
        content: "/model jev-decision-test".to_string(),
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
    };
    assert!(handle_runtime_command_if_needed(&ctx, &generation, &msg, Some(&channel)).await);
    let sent = channel_impl.sent_messages.lock().await;
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].contains("cannot be used for chat") && !sent[0].contains("Model switched"),
        "a decision-only model was accepted: {}",
        sent[0]
    );
    let key = ConversationKey::from_message(&msg);
    assert_eq!(
        get_route_selection_for_generation(&ctx, &key, &generation).model,
        "vendor/chat-model",
        "the route is unchanged"
    );
}

/// R-PMMC F-1: a `hint:` default starts the channel runtime again.
#[test]
fn a_hint_default_passes_the_channel_start_up_check() {
    let mut config = decision_only_config("openrouter");
    config.default_model = Some("hint:fast".to_string());
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: "openrouter".to_string(),
        model: "vendor/fast-chat".to_string(),
        api_key: None,
        credential_ref: None,
    });
    check_channel_startup_default(&config).expect("a hint default passes");
}

/// R-PMMC F-6.1: `/models <provider>` resolves the new provider with the
/// sender's current model before it switches.
#[tokio::test]
async fn a_provider_switch_that_cannot_serve_the_current_model_is_refused() {
    let mut config = decision_only_config("openrouter");
    config.default_model = Some("vendor-model".to_string());
    config.providers.models.push(
        toml::from_str("provider = \"openai\"\nmodel = \"vendor-model\"\noperations = [\"embedding\"]\n")
            .expect("test: declaration"),
    );
    let shared = crate::config::new_shared(config);
    let generation = shared.pin();
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = shared.clone();
    ctx.config_generation = Arc::clone(&generation);
    ctx.default_provider = Arc::new("openrouter".to_string());
    ctx.provider_cache.lock().insert(
        (generation.id.0, "openai".to_string(), "vendor-model".to_string()),
        Arc::new(DummyProvider) as Arc<dyn Provider>,
    );
    let msg = traits::ChannelMessage {
        id: "msg-models".to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-1".to_string(),
        content: "/models openai".to_string(),
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
    };
    assert!(handle_runtime_command_if_needed(&ctx, &generation, &msg, Some(&channel)).await);
    let sent = channel_impl.sent_messages.lock().await;
    assert!(
        sent[0].contains("cannot be used for chat") && !sent[0].contains("Provider switched"),
        "{}",
        sent[0]
    );
    let key = ConversationKey::from_message(&msg);
    assert_eq!(
        get_route_selection_for_generation(&ctx, &key, &generation).provider,
        "openrouter"
    );
}

#[tokio::test]
async fn a_model_switch_to_a_hint_is_accepted() {
    let mut config = decision_only_config("openrouter");
    config.default_model = Some("vendor/chat-model".to_string());
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: "openrouter".to_string(),
        model: "vendor/fast-chat".to_string(),
        api_key: None,
        credential_ref: None,
    });
    let shared = crate::config::new_shared(config);
    let generation = shared.pin();
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = shared.clone();
    ctx.config_generation = Arc::clone(&generation);
    ctx.default_provider = Arc::new("openrouter".to_string());
    let msg = traits::ChannelMessage {
        id: "msg-model-hint".to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-1".to_string(),
        content: "/model hint:fast".to_string(),
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
    };
    assert!(handle_runtime_command_if_needed(&ctx, &generation, &msg, Some(&channel)).await);
    let sent = channel_impl.sent_messages.lock().await;
    assert!(sent[0].contains("Model switched"), "{}", sent[0]);
    assert!(!sent[0].contains("will be refused"), "{}", sent[0]);
}

#[tokio::test]
async fn channel_default_hint_turn_uses_only_selected_route_key_on_wire() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
    let temp = TempDir::new().expect("test: temp workspace");
    let root = CredentialProbe::start().await;
    let route = CredentialProbe::start().await;
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some(root.custom_provider());
    config.default_model = Some("hint:fast".to_string());
    config.api_key = Some("PRX-CHANNEL-ROOT-KEY".to_string());
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: route.custom_provider(),
        model: PROBE_MODEL.to_string(),
        api_key: Some("PRX-CHANNEL-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    config.cost.prices.insert(
        format!("{}/{PROBE_MODEL}", route.custom_provider()),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let manager = crate::config::new_shared(config);
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = Arc::clone(&manager);
    ctx.config_generation = manager.pin();
    ctx.default_provider = Arc::new(root.custom_provider());
    ctx.model = Arc::new("hint:fast".to_string());
    ctx.workspace_dir = Arc::new(temp.path().to_path_buf());
    ctx.provider_runtime_options = isolated_provider_runtime_options(&temp);
    ctx.channels_by_name = Arc::new(HashMap::from([(channel.name().to_string(), channel)]));
    run_owned_channel_probe(
        Arc::new(ctx),
        traits::ChannelMessage {
            id: "prx-hint-channel-turn".to_string(),
            sender: "prx_user".to_string(),
            reply_target: "prx_channel".to_string(),
            content: "probe".to_string(),
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
    assert_eq!(route.wait_for_requests(1, std::time::Duration::from_secs(10)).await, 1);
    assert!(route.saw("PRX-CHANNEL-ROUTE-KEY"));
    assert!(!route.saw("PRX-CHANNEL-ROOT-KEY"));
    assert!(root.requests().is_empty());
}

/// R-PMMCh C-2: a model quoted with padding inside the backticks is
/// switched to (and kept on the route) trimmed.
#[tokio::test]
async fn a_model_switch_stores_the_model_trimmed() {
    let mut config = decision_only_config("openrouter");
    config.default_model = Some("vendor/chat-model".to_string());
    let shared = crate::config::new_shared(config);
    let generation = shared.pin();
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = shared.clone();
    ctx.config_generation = Arc::clone(&generation);
    ctx.default_provider = Arc::new("openrouter".to_string());
    let msg = traits::ChannelMessage {
        id: "msg-model-padded".to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-1".to_string(),
        content: "/model ` vendor/other-chat `".to_string(),
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
    };
    assert!(handle_runtime_command_if_needed(&ctx, &generation, &msg, Some(&channel)).await);
    let sent = channel_impl.sent_messages.lock().await;
    assert!(sent[0].contains("Model switched to `vendor/other-chat`"), "{}", sent[0]);
    let route = get_route_selection_for_generation(&ctx, &ConversationKey::from_message(&msg), &generation);
    assert_eq!(route.model, "vendor/other-chat");
}

#[test]
fn a_decision_only_default_does_not_start_the_channel_runtime() {
    let config = decision_only_config("openrouter");
    let Err(error) = check_channel_startup_default(&config) else {
        panic!("a decision-only default passed the channel start-up check");
    };
    let text = format!("{error:#}");
    assert!(text.contains("cannot be used for chat"), "{text}");
    assert!(!text.contains("PMMC-CHANNEL-ROOT"), "{text}");
}

#[test]
fn a_provider_switch_keeps_the_region_it_names() {
    assert_eq!(resolve_provider_alias("qwen-us").as_deref(), Some("qwen-us"));
    assert_eq!(resolve_provider_alias(" Kimi ").as_deref(), Some("moonshot"));
    assert_eq!(resolve_provider_alias("OpenRouter").as_deref(), Some("openrouter"));
    assert_eq!(
        resolve_provider_alias("custom:http://127.0.0.1:9/v1"),
        None,
        "no URL from a chat message"
    );
    assert_eq!(resolve_provider_alias("no-such-provider"), None);
}
