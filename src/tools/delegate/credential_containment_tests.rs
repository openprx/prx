use super::*;
use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
use serde_json::json;

const AGENT_SENTINEL: &str = "C0-DELEGATE-AGENT-SENTINEL";
const ROOT_SENTINEL: &str = "C0-DELEGATE-ROOT-SENTINEL";

fn agent(provider: &str, api_key: Option<&str>) -> DelegateAgentConfig {
    DelegateAgentConfig {
        provider: provider.to_string(),
        model: PROBE_MODEL.to_string(),
        system_prompt: None,
        api_key: api_key.map(str::to_string),
        credential_ref: None,
        temperature: None,
        max_depth: 3,
        agentic: false,
        allowed_tools: Vec::new(),
        identity_dir: None,
        memory_scope: None,
        spawn_enabled: None,
        enabled: true,
        autonomy: None,
    }
}

fn security(temp: &tempfile::TempDir) -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy {
        workspace_dir: temp.path().to_path_buf(),
        ..SecurityPolicy::default()
    })
}

fn fixture_price() -> crate::config::schema::ModelPricing {
    crate::config::schema::ModelPricing {
        input: 1.0,
        output: 1.0,
        cache_write: 1.0,
        cache_read: 1.0,
    }
}

fn admitted_config(temp: &tempfile::TempDir, agents: HashMap<String, DelegateAgentConfig>) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = temp.path().to_owned();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some("ollama".to_owned());
    config.default_model = Some("prx-local-model".to_owned());
    config.causal_tree.enabled = false;
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.reliability.provider_retries = 0;
    for agent in agents.values() {
        config
            .cost
            .prices
            .insert(format!("{}/{}", agent.provider, agent.model.trim()), fixture_price());
    }
    let default_provider = config.default_provider.as_deref().expect("test: default provider");
    for model in agents.values().map(|agent| agent.model.trim()) {
        config
            .cost
            .prices
            .insert(format!("{default_provider}/{model}"), fixture_price());
    }
    config.agents = agents;
    config
}

/// PMM-C: a delegation to a model that cannot chat is refused before any
/// provider exists, whether the agent config or the call names it.
#[tokio::test]
async fn a_delegation_to_a_decision_only_model_is_refused_before_transport() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let own = CredentialProbe::start().await;
    let mut decision = agent(&own.custom_provider(), Some(AGENT_SENTINEL));
    decision.model = "jev-decision-test".to_string();
    let agents = HashMap::from([("a".to_string(), decision)]);
    let mut config = admitted_config(&temp, agents.clone());
    config
        .providers
        .models
        .push(crate::config::schema::ProviderModelConfig {
            provider: own.custom_provider(),
            model: Some("jev-decision-test".to_owned()),
            operations: Some(vec![crate::config::schema::ModelOperationSetting::DecisionEndpoint]),
            native_tools: None,
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema_dialect: None,
            tool_call_dialect: None,
            context_window: None,
        });
    let tool = DelegateTool::new(agents, security(&temp));
    let result = super::tests::dispatch_admitted_delegate(config, tool, json!({"agent": "a", "prompt": "hi"})).await;
    assert!(!result.success, "a decision-only model served a delegation");
    let error = result.error.unwrap_or_default();
    assert!(error.contains("cannot be used for chat"), "{error}");
    assert!(own.requests().is_empty(), "nothing is sent");
    assert!(!error.contains(AGENT_SENTINEL));
}

/// A `custom:<url>` the operator never configured is refused, and the
/// named agent's key never reaches it.
#[tokio::test]
async fn a_model_named_endpoint_the_operator_did_not_configure_is_refused() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let own = CredentialProbe::start().await;
    let foreign = CredentialProbe::start().await;
    let agents = HashMap::from([("a".to_string(), agent(&own.custom_provider(), Some(AGENT_SENTINEL)))]);
    let config = admitted_config(&temp, agents.clone());
    let tool = DelegateTool::new(agents, security(&temp));
    let result = super::tests::dispatch_admitted_delegate(
        config,
        tool,
        json!({"agent": "a", "prompt": "hi", "provider": foreign.custom_provider()}),
    )
    .await;

    assert!(!result.success, "an unconfigured endpoint must be refused");
    let error = result.error.unwrap_or_default();
    assert!(error.contains("not configured"), "{error}");
    assert!(
        foreign.requests().is_empty(),
        "nothing may reach an unconfigured endpoint"
    );
    assert!(!foreign.saw(AGENT_SENTINEL));
    assert!(own.requests().is_empty());
}

/// A configured provider named per call gets only its own credential: the
/// named agent's key stays with the agent's own provider.
#[tokio::test]
async fn a_named_agent_key_stays_with_its_own_provider() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let own = CredentialProbe::start().await;
    let other = CredentialProbe::start().await;
    let agents = HashMap::from([
        ("a".to_string(), agent(&own.custom_provider(), Some(AGENT_SENTINEL))),
        ("b".to_string(), agent(&other.custom_provider(), None)),
    ]);
    let config = admitted_config(&temp, agents.clone());
    let tool = DelegateTool::new(agents, security(&temp));
    let result = super::tests::dispatch_admitted_delegate(
        config,
        tool,
        json!({"agent": "a", "prompt": "hi", "provider": other.custom_provider()}),
    )
    .await;

    assert!(
        result.success,
        "a configured keyless endpoint serves: {:?}",
        result.error
    );
    let requests = other.requests();
    assert_eq!(requests.len(), 1);
    assert!(!other.saw(AGENT_SENTINEL), "agent a's key reached another provider");
    assert!(requests.iter().all(|request| request.authorization().is_none()));
}

#[tokio::test]
async fn delegated_hint_uses_only_its_selected_route_key_on_wire() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let root = CredentialProbe::start().await;
    let route = CredentialProbe::start().await;
    let mut named = agent(&root.custom_provider(), Some("PRX-DELEGATE-ROOT-KEY"));
    named.model = "hint:fast".to_string();
    let agents = HashMap::from([("prx_agent".to_string(), named)]);
    let mut config = admitted_config(&temp, agents.clone());
    config.default_provider = Some(root.custom_provider());
    config.api_key = Some("PRX-DELEGATE-ROOT-KEY".to_string());
    config.agents = agents.clone();
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: route.custom_provider(),
        model: PROBE_MODEL.to_string(),
        api_key: Some("PRX-DELEGATE-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    config
        .cost
        .prices
        .insert(format!("{}/{}", route.custom_provider(), PROBE_MODEL), fixture_price());
    let tool = DelegateTool::new(agents, security(&temp))
        .with_hint_config(&config)
        .with_model_catalog(providers::selection::catalog_for(&config))
        .with_configured_providers(&providers::ConfiguredProviders::from_config(&config));
    let result =
        super::tests::dispatch_admitted_delegate(config, tool, json!({"agent": "prx_agent", "prompt": "probe"})).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(route.wait_for_requests(1, std::time::Duration::from_secs(10)).await, 1);
    assert!(route.saw("PRX-DELEGATE-ROUTE-KEY"));
    assert!(!route.saw("PRX-DELEGATE-ROOT-KEY"));
    assert!(root.requests().is_empty());
}

/// Guard: without an override the named agent presents its own key, and
/// the root key goes only to the provider it was configured for.
#[tokio::test]
async fn own_credentials_still_reach_their_own_providers() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let own = CredentialProbe::start().await;
    let root = CredentialProbe::start().await;
    let agents = HashMap::from([
        ("a".to_string(), agent(&own.custom_provider(), Some(AGENT_SENTINEL))),
        ("b".to_string(), agent(&root.custom_provider(), None)),
    ]);
    let mut config = admitted_config(&temp, agents.clone());
    config.default_provider = Some(root.custom_provider());
    config.api_key = Some(ROOT_SENTINEL.to_owned());
    let first_tool = DelegateTool::new(agents.clone(), security(&temp));
    let second_tool = DelegateTool::new(agents, security(&temp));
    let first = super::tests::dispatch_admitted_delegate(
        config.clone(),
        first_tool,
        json!({"agent": "a", "prompt": "first credential probe"}),
    )
    .await;
    let second = super::tests::dispatch_admitted_delegate(
        config,
        second_tool,
        json!({"agent": "b", "prompt": "second credential probe"}),
    )
    .await;

    assert!(first.success && second.success, "{:?} {:?}", first.error, second.error);
    assert!(own.saw(AGENT_SENTINEL) && !own.saw(ROOT_SENTINEL));
    assert!(root.saw(ROOT_SENTINEL) && !root.saw(AGENT_SENTINEL));
}

// ── C0-f F-1: the root key travels only with the root URL ─────────────

const F1_ROOT_SENTINEL: &str = "C0F-DELEGATE-ROOT-SENTINEL";

fn root_bound_tool(temp: &tempfile::TempDir, default_provider: &str, api_url: &str) -> DelegateTool {
    let agents = HashMap::from([("a".to_string(), agent("groq", None))]);
    DelegateTool::new(agents, security(temp)).with_root_connection(RootConnection {
        provider: Some(default_provider.to_string()),
        key: Some(F1_ROOT_SENTINEL.to_string()),
        api_url: Some(api_url.to_string()),
    })
}

/// Reviewer scenario: the root key of a `litellm` default configured with
/// its own base URL. A model-chosen spelling of the same provider must get
/// the key and the URL together, never the key on the catalog default host.
#[test]
fn a_respelled_default_gets_the_root_key_only_with_the_root_url() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let tool = root_bound_tool(&temp, "litellm", "http://127.0.0.1:9/litellm");
    let child = agent("groq", None);
    assert_eq!(
        providers::canonical_provider_key("lite-llm"),
        providers::canonical_provider_key("litellm")
    );

    for spelling in ["litellm", "lite-llm", "LiteLLM"] {
        let connection = tool.child_connection(&child, spelling);
        assert_eq!(connection.api_key, Some(F1_ROOT_SENTINEL), "{spelling}");
        assert_eq!(connection.api_url, Some("http://127.0.0.1:9/litellm"), "{spelling}");
        assert!(connection.released, "{spelling}");
    }
}

/// INT-2B a33 (R-C0f N-5, D-030), RED before: the root connection was
/// released to a child only when the root had a key, so a keyless
/// default (a remote Ollama given by `api_url`) lost its base URL and the
/// child went to the catalog default host, while the worker path kept
/// it. After: the root URL is released to the same provider identity
/// with or without a key; another provider still gets nothing.
#[test]
fn a_keyless_root_default_keeps_its_base_url() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let tool = DelegateTool::new(HashMap::new(), security(&temp)).with_root_connection(RootConnection {
        provider: Some("ollama".to_string()),
        key: None,
        api_url: Some("http://127.0.0.1:9/ollama".to_string()),
    });
    let child = agent("ollama", None);

    let connection = tool.child_connection(&child, "ollama");
    assert_eq!(connection.api_url, Some("http://127.0.0.1:9/ollama"));
    assert_eq!(connection.api_key, None);
    assert!(connection.released);

    let groq = agent("groq", None);
    let other = tool.child_connection(&groq, "groq");
    assert_eq!(other.api_url, None);
    assert!(!other.released);
}

/// B4f (R-INT2BB4 report L-9): pins a33's accepted rotation-key change.
/// The root rotation keys (`reliability.api_keys`) follow the keyless
/// root connection to a child on the same provider identity, and a child
/// on another identity gets none. Checked on the scoped settings and on
/// the wire: the root endpoint answers the keyless first request `429`,
/// so the chain rotates to the rotation key, which that endpoint then
/// sees; the other endpoint never sees it.
#[tokio::test]
async fn a_keyless_root_releases_its_rotation_keys_only_to_its_own_identity() {
    const ROTATION_SENTINEL: &str = "B4F-ROTATION-KEY-SENTINEL";
    let temp = tempfile::tempdir().expect("test: temp dir");
    let root = CredentialProbe::start_with_statuses(vec![429]).await;
    let other = CredentialProbe::start_with_statuses(vec![429]).await;
    let agents = HashMap::from([
        ("same".to_string(), agent(&root.custom_provider(), None)),
        ("other".to_string(), agent(&other.custom_provider(), None)),
    ]);
    let reliability = crate::config::ReliabilityConfig {
        provider_retries: 0,
        provider_backoff_ms: 0,
        api_keys: vec![ROTATION_SENTINEL.to_string()],
        ..crate::config::ReliabilityConfig::default()
    };
    let mut config = admitted_config(&temp, agents.clone());
    config.default_provider = Some(root.custom_provider());
    config.api_key = None;
    config.reliability = reliability.clone();
    let other_tool = DelegateTool::new(agents.clone(), security(&temp));
    let tool = DelegateTool::new(agents, security(&temp))
        .with_root_connection(RootConnection {
            provider: Some(root.custom_provider()),
            key: None,
            api_url: None,
        })
        .with_reliability(reliability);

    // The scoped settings the child chain is built with.
    let same_child = agent(&root.custom_provider(), None);
    let same = tool.child_connection(&same_child, &root.custom_provider());
    assert!(same.released);

    // On the wire.
    let served = super::tests::dispatch_admitted_delegate(
        config.clone(),
        tool,
        json!({"agent": "same", "prompt": "root rotation probe"}),
    )
    .await;
    assert!(served.success, "{:?}", served.error);
    assert!(root.saw(ROTATION_SENTINEL), "the rotation key reached its own identity");
    let _ = super::tests::dispatch_admitted_delegate(
        config,
        other_tool,
        json!({"agent": "other", "prompt": "foreign rotation probe"}),
    )
    .await;
    assert!(!other.requests().is_empty(), "the other endpoint was reached");
    assert!(
        !other.saw(ROTATION_SENTINEL),
        "another identity never gets the rotation key"
    );
}

/// The `openai` + corporate gateway variant.
#[test]
fn a_respelled_openai_default_keeps_its_corporate_url() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let tool = root_bound_tool(&temp, "openai", "https://gateway.corp.example/v1");
    let child = agent("groq", None);

    for spelling in ["openai", "OpenAI"] {
        let connection = tool.child_connection(&child, spelling);
        assert_eq!(connection.api_key, Some(F1_ROOT_SENTINEL), "{spelling}");
        assert_eq!(
            connection.api_url,
            Some("https://gateway.corp.example/v1"),
            "{spelling}"
        );
    }
}

/// A named agent's own key is its own credential, with no root URL and no
/// root rotation keys; another provider gets nothing.
#[test]
fn an_agent_key_never_borrows_the_root_url() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let tool = root_bound_tool(&temp, "litellm", "http://127.0.0.1:9/litellm");
    let child = agent("litellm", Some(AGENT_SENTINEL));

    let own = tool.child_connection(&child, "lite-llm");
    assert_eq!(own.api_key, Some(AGENT_SENTINEL));
    assert_eq!(own.api_url, None);
    assert!(!own.released);

    let groq = agent("groq", None);
    let other = tool.child_connection(&groq, "mistral");
    assert_eq!((other.api_key, other.api_url, other.released), (None, None, false));
}

/// On the wire: a respelled default reaches the root URL with the root key.
#[tokio::test]
async fn f1_wire_a_respelled_default_reaches_only_the_root_url() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let root = CredentialProbe::start().await;
    let tool = root_bound_tool(&temp, "litellm", &root.base_url());

    let mut config = admitted_config(&temp, tool.agents.as_ref().clone());
    config.default_provider = Some("litellm".to_owned());
    config.api_key = Some(F1_ROOT_SENTINEL.to_owned());
    config.api_url = Some(root.base_url());
    config
        .cost
        .prices
        .insert(format!("lite-llm/{PROBE_MODEL}"), fixture_price());
    let result = super::tests::dispatch_admitted_delegate(
        config,
        tool,
        json!({"agent": "a", "prompt": "hi", "provider": "lite-llm"}),
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(root.requests().len(), 1);
    assert!(root.saw(F1_ROOT_SENTINEL));
}

/// USERINFO-2: the delegate tool names its provider in the output the
/// parent model reads, in its failure text and in its durable events. A
/// `custom:` URL may carry `user:password@`; none of them may carry it,
/// while the request still reaches the endpoint.
#[tokio::test]
async fn delegate_names_its_provider_without_url_userinfo() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let probe = CredentialProbe::start().await;
    let label = probe
        .custom_provider()
        .replacen("http://", "http://u:SECRET-fixture-pw@", 1);
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
    let agents = HashMap::from([
        ("a".to_string(), agent(&label, Some(AGENT_SENTINEL))),
        (
            "broken".to_string(),
            agent(
                "custom:http://u:SECRET-fixture-pw@127.0.0.1:99999/v1",
                Some(AGENT_SENTINEL),
            ),
        ),
    ]);
    let config = admitted_config(&temp, agents.clone());
    let success_tool = DelegateTool::new(agents.clone(), security(&temp)).with_shared_memory(Arc::clone(&memory));
    let failure_tool = DelegateTool::new(agents, security(&temp)).with_shared_memory(Arc::clone(&memory));
    let served = super::tests::dispatch_admitted_delegate(
        config.clone(),
        success_tool,
        json!({"agent": "a", "prompt": "provider label success"}),
    )
    .await;
    assert!(served.success, "{served:?}");
    assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
    assert!(served.output.contains("[redacted]@127.0.0.1"), "{}", served.output);
    assert!(!served.output.contains("SECRET-fixture-pw"), "{}", served.output);

    let failed = super::tests::dispatch_admitted_delegate(
        config,
        failure_tool,
        json!({"agent": "broken", "prompt": "provider label failure"}),
    )
    .await;
    assert!(!failed.success, "invalid provider must fail");
    let error = failed.error.unwrap_or_default();
    assert!(error.contains("is not one PRX can construct"), "{error}");
    assert!(!error.contains("SECRET-fixture-pw"), "{error}");

    let events = memory
        .list_message_events_recent(
            &crate::memory::MemoryPrincipal {
                require_owner_match: false,
                workspace_id: temp.path().to_string_lossy().to_string(),
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
                agent_partition: None,
            },
            100,
        )
        .await
        .expect("test: events readable");
    assert!(
        events.iter().any(|event| event.event_type == "turn.finalized"),
        "the successful delegate must persist its terminal event"
    );
    let stored = serde_json::to_string(&events).expect("test: events serialize");
    assert!(
        !stored.contains("SECRET-fixture-pw"),
        "a durable event kept the password: {stored}"
    );
}

// ── D-053 / REDACT-TURN persistence probes ────────────────────────────

/// Every event the delegate recorded under the temp workspace.
async fn workspace_events(memory: &Arc<dyn Memory>, temp: &tempfile::TempDir) -> Vec<crate::memory::MessageEvent> {
    memory
        .list_message_events_recent(
            &crate::memory::MemoryPrincipal {
                require_owner_match: false,
                workspace_id: temp.path().to_string_lossy().to_string(),
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
                agent_partition: None,
            },
            100,
        )
        .await
        .expect("test: events readable")
}

/// D-053 / REDACT-TURN probe: a non-agentic delegation whose provider reply
/// carries a secret commits through `finalize_delegate_turn` (success
/// branch) and the result/task records; nothing under the workspace may
/// keep the secret. It must fail before the REDACT-TURN commits.
#[tokio::test]
async fn delegate_persists_no_secret_from_a_reply() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};

    let temp = tempfile::tempdir().expect("test: temp dir");
    let probe = CredentialProbe::start_replying(&format!("Here is the key {PROBE_SECRET} you asked for")).await;
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
    let agents = HashMap::from([("a".to_string(), agent(&probe.custom_provider(), None))]);
    let config = admitted_config(&temp, agents.clone());
    let tool = DelegateTool::new(agents, security(&temp)).with_shared_memory(Arc::clone(&memory));
    let served = super::tests::dispatch_admitted_delegate(config, tool, json!({"agent": "a", "prompt": "hi"})).await;
    assert!(served.success, "{served:?}");
    assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
    assert!(
        served.output.contains(PROBE_SECRET),
        "the reply must carry the secret for the probe to mean anything: {}",
        served.output
    );
    let events = workspace_events(&memory, &temp).await;
    assert!(
        events.iter().any(|event| event.event_type == "turn.finalized"),
        "the delegate must reach the terminal commit: {events:?}"
    );

    // The workspace, the SQLite memory and the provider runtime dir are
    // all this temp dir.
    assert_nothing_under_persists_the_probe_secret(temp.path());
}

/// D-053 / REDACT-TURN probe: a non-agentic delegation whose provider fails
/// with an error carrying a secret commits through `finalize_delegate_turn`
/// (failure branch) and the result/task records; nothing under the
/// workspace may keep the secret. It must fail before the REDACT-TURN commits.
#[tokio::test]
async fn delegate_persists_no_secret_from_a_provider_error() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};

    let temp = tempfile::tempdir().expect("test: temp dir");
    let probe = CredentialProbe::start_failing(400, &format!("upstream rejected key {PROBE_SECRET}")).await;
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
    let agents = HashMap::from([("a".to_string(), agent(&probe.custom_provider(), None))]);
    let config = admitted_config(&temp, agents.clone());
    let tool = DelegateTool::new(agents, security(&temp)).with_shared_memory(Arc::clone(&memory));
    let failed = super::tests::dispatch_admitted_delegate(config, tool, json!({"agent": "a", "prompt": "hi"})).await;
    assert!(!failed.success, "a failing upstream must fail the delegation");
    assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
    let error = failed.error.unwrap_or_default();
    // The upstream's error text must reach the turn: verbatim before the
    // redactors merged (D-054), already redacted by the provider layer since.
    assert!(
        error.contains(PROBE_SECRET) || error.contains("upstream rejected key [REDACTED]"),
        "the provider error must reach the turn for the probe to mean anything: {error}"
    );
    let events = workspace_events(&memory, &temp).await;
    assert!(
        events.iter().any(|event| event.event_type == "turn.finalized"),
        "the delegate must reach the terminal commit: {events:?}"
    );

    // The workspace, the SQLite memory and the provider runtime dir are
    // all this temp dir.
    assert_nothing_under_persists_the_probe_secret(temp.path());
}

/// Loopback OpenAI-compatible endpoint that keeps every chat request body
/// (unlike `CredentialProbe`, which retains no prompt content).
async fn capturing_endpoint() -> (
    String,
    Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
    let capture = Arc::clone(&requests);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind capture endpoint");
    let addr = listener.local_addr().expect("test: capture endpoint address");
    let chat = axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| {
        let capture = Arc::clone(&capture);
        async move {
            capture.lock().push(request);
            axum::Json(json!({
                "id": "chatcmpl-capture",
                "object": "chat.completion",
                "created": 1,
                "model": PROBE_MODEL,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "CAPTURE-DONE"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }))
        }
    });
    let models = axum::routing::get(|| async {
        axum::Json(json!({
            "object": "list",
            "data": [{"id": PROBE_MODEL, "object": "model", "created": 1, "owned_by": "probe"}]
        }))
    });
    let app = axum::Router::new()
        .route("/v1/chat/completions", chat)
        .route("/v1/models", models);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/v1"), requests, server)
}

/// One ordinary delegate turn through the admitted path; returns its
/// recorded input event.
async fn sibling_lane(
    temp: &tempfile::TempDir,
    memory: &Arc<dyn Memory>,
    agents: &HashMap<String, DelegateAgentConfig>,
    sentinel: &str,
) -> crate::memory::MessageEvent {
    let config = admitted_config(temp, agents.clone());
    assert!(!config.causal_tree.enabled);
    let tool = DelegateTool::new(agents.clone(), security(temp)).with_shared_memory(Arc::clone(memory));
    let args = json!({"agent": "a", "prompt": "lane a", "context": sentinel});
    let result = super::tests::dispatch_admitted_delegate(config, tool, args).await;
    assert!(result.success, "{:?}", result.error);
    workspace_events(memory, temp)
        .await
        .into_iter()
        .find(|event| event.role == "user" && event.source == "delegate" && event.content.contains(sentinel))
        .expect("test: the delegate input must be recorded")
}

/// Seeds a durable event that belongs to the same owner, session, channel,
/// sender and agent as `origin`, so it is in scope for that lane's recall.
fn sibling_scope(
    origin: &crate::memory::MessageEvent,
    source: crate::memory::MessageEventSource,
    run_id: Option<&str>,
    subject: Option<crate::memory::MessageEventSubject>,
) -> MessageEventScope {
    let mut scope = MessageEventScope::new(source, crate::memory::MemoryVisibility::Workspace);
    scope.owner_id = origin.owner_id.clone();
    scope.session_key = origin.session_key.clone();
    scope.channel = origin.channel.clone();
    scope.sender = origin.sender.clone();
    scope.agent_id = origin.agent_id.clone();
    scope.run_id = run_id.map(str::to_owned);
    scope.subject = subject;
    scope
}

/// Two sibling delegates from the same parent loop, admitted through the
/// real ordinary path, record distinct task identities. A sibling's prior
/// segment never reaches the other child's provider request, while a shared
/// main-channel event does.
#[tokio::test]
async fn ordinary_sibling_delegates_get_distinct_task_identities_in_admitted_requests() {
    use crate::memory::{MessageEventSource, MessageEventSubject};

    let temp = tempfile::tempdir().expect("test: temp dir");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
    let workspace = temp.path().to_string_lossy().to_string();
    let (base, requests, server) = capturing_endpoint().await;
    let agents = HashMap::from([("a".to_string(), agent(&format!("custom:{base}"), Some(AGENT_SENTINEL)))]);

    let a_in = sibling_lane(&temp, &memory, &agents, "A2-SIBLING-A-SENTINEL").await;
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.clone());
    fabric
        .record_inbound_user_message(
            sibling_scope(
                &a_in,
                MessageEventSource::Delegate,
                Some("child-segment:a2:1"),
                a_in.run_id.clone().map(MessageEventSubject::Task),
            ),
            "A2-SIBLING-A-SEGMENT",
            None,
            None,
        )
        .await
        .expect("test: sibling segment record");
    fabric
        .record_inbound_user_message(
            sibling_scope(&a_in, MessageEventSource::Channel, None, None),
            "A2-SHARED-POSITIVE",
            None,
            None,
        )
        .await
        .expect("test: shared record");

    let b_in = sibling_lane(&temp, &memory, &agents, "A2-SIBLING-B-SENTINEL").await;
    let bodies = requests.lock().clone();
    server.abort();

    for lane in [&a_in, &b_in] {
        assert_eq!(
            lane.subject,
            Some(MessageEventSubject::Task(lane.run_id.clone().expect("test: lane run")))
        );
    }
    assert_ne!(a_in.run_id, b_in.run_id, "sibling delegates must not share a run");
    assert_eq!(a_in.session_key, b_in.session_key);
    assert_eq!(a_in.owner_id, b_in.owner_id);

    let b_bodies: Vec<String> = bodies
        .iter()
        .map(|body| body.to_string())
        .filter(|body| body.contains("A2-SIBLING-B-SENTINEL"))
        .collect();
    assert!(!b_bodies.is_empty(), "sibling B must reach the provider");
    assert!(
        b_bodies.iter().all(|body| !body.contains("A2-SIBLING-A-SEGMENT")),
        "sibling A's segment reached sibling B's provider request"
    );
    assert!(
        b_bodies.iter().any(|body| body.contains("A2-SHARED-POSITIVE")),
        "the shared main event must reach sibling B's provider request"
    );
}
