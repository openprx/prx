#![cfg(test)]

use super::*;

// ── C0-C: model-chosen provider credential containment ──────────────

const SPAWN_AGENT_SENTINEL: &str = "C0-SPAWN-AGENT-SENTINEL";
const SPAWN_ROOT_SENTINEL: &str = "C0-SPAWN-ROOT-SENTINEL";

fn containment_agent(provider: &str, api_key: Option<&str>) -> DelegateAgentConfig {
    DelegateAgentConfig {
        provider: provider.to_string(),
        model: crate::providers::credential_probe::PROBE_MODEL.to_string(),
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
    }
}

fn containment_tool(
    temp: &tempfile::TempDir,
    agents: HashMap<String, DelegateAgentConfig>,
    gateway_provider: &str,
    fallback_api_key: Option<&str>,
) -> SessionsSpawnTool {
    let (channel, _sent) = RecordingChannel::new();
    SessionsSpawnTool::new(
        Arc::new(channel),
        gateway_provider,
        crate::providers::credential_probe::PROBE_MODEL,
        0.0,
        Arc::new(SecurityPolicy {
            workspace_dir: temp.path().to_path_buf(),
            ..SecurityPolicy::default()
        }),
        temp.path().to_path_buf(),
        MultimodalConfig::default(),
        agents,
        fallback_api_key.map(str::to_string),
        providers::ProviderRuntimeOptions {
            openprx_dir: Some(temp.path().to_path_buf()),
            codex_auth_json_path: Some(temp.path().join("no-codex-auth.json")),
            codex_auth_json_auto_import: false,
            ..providers::ProviderRuntimeOptions::default()
        },
        crate::config::SessionsSpawnConfig::default(),
    )
    .with_test_parent_provider(Arc::new(EchoProvider {
        response: "gateway".into(),
    }))
}

/// Finding 71: a named-agent override must retain the model fallback
/// contract when the admitted spawn builds its child provider. The
/// incompatible fallback is refused before the run or transport exists.
#[tokio::test]
async fn admitted_named_agent_spawn_refuses_a_cross_dialect_fallback_before_transport() {
    use crate::providers::credential_probe::CredentialProbe;

    let temp = tempfile::tempdir().expect("test: temp dir");
    let probe = CredentialProbe::start().await;
    let agent_provider = probe.custom_provider();
    let mut agent = containment_agent(&agent_provider, Some(SPAWN_AGENT_SENTINEL));
    agent.model = crate::providers::entrypoint_consistency_tests::PRIMARY_MODEL.to_string();
    let agents = HashMap::from([("prx_agent".to_string(), agent)]);

    let mut config = crate::config::Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = temp.path().join("no-codex-auth.json");
    config.secrets.encrypt = false;
    crate::providers::entrypoint_consistency_tests::configure_cross_dialect(&mut config, &agent_provider);
    let catalog = providers::selection::catalog_for(&config);
    let configured = providers::ConfiguredProviders::from_config(&config);
    let tool = containment_tool(&temp, agents, "ollama", None)
        .with_hint_config(&config)
        .with_model_catalog(catalog)
        .with_configured_providers(&configured);

    let refusal =
        match execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "probe", "agent": "prx_agent"}))).await {
            Ok(result) => {
                assert!(!result.success, "the incompatible agent fallback was spawned");
                result.error.unwrap_or_default()
            }
            Err(error) => format!("{error:#}"),
        };
    crate::providers::entrypoint_consistency_tests::assert_cross_dialect_refusal(
        "sessions_spawn named agent",
        &refusal,
    );
    assert!(probe.requests().is_empty(), "a refused fallback reached transport");
    assert!(
        tool.active_runs_snapshot().await.is_empty(),
        "a refused fallback created a run"
    );
}

/// The admitted child binding, rather than the removed local reconstruction
/// shim, owns retries for a named provider override.
#[tokio::test]
async fn provider_override_goes_through_the_reliability_layer() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};

    let temp = tempfile::tempdir().expect("test: temp dir");
    let probe = CredentialProbe::start_with_statuses(vec![429, 429]).await;
    let provider_name = probe.custom_provider();
    let agents = HashMap::from([(
        "prx_agent".to_string(),
        containment_agent(&provider_name, Some("PRX-CHILD-RETRY-CREDENTIAL")),
    )]);
    let mut tool = containment_tool(&temp, agents, "ollama", None);
    let mut config = preconstruction_parent_config(&tool, temp.path());
    config.reliability.provider_retries = 2;
    config.reliability.provider_backoff_ms = 1;
    config.providers.models.push(
        serde_json::from_value(json!({
            "provider": provider_name, "model": PROBE_MODEL,
            "operations": ["chat"], "native_tools": true
        }))
        .expect("test: child model declaration"),
    );
    config.cost.prices.insert(
        format!("{provider_name}/{PROBE_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let catalog = providers::selection::catalog_for(&config);
    tool = tool
        .with_hint_config(&config)
        .with_model_catalog(catalog)
        .with_configured_providers(&providers::ConfiguredProviders::from_config(&config));
    let runs = tool.active_runs_arc();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: parent memory"));
    let result = dispatch_preconstruction_parent(
        tool,
        config,
        memory,
        with_spawn_grant(json!({"task": "probe", "agent": "prx_agent"})),
    )
    .await
    .expect("test: admitted retry spawn");
    assert!(result.success, "named child was not started: {:?}", result.error);
    assert_eq!(probe.wait_for_requests(3, std::time::Duration::from_secs(10)).await, 3);
    let run_id = runs.read().await.first().expect("test: child run").id.clone();
    assert_eq!(
        await_terminal_reason(&runs, &run_id).await,
        crate::providers::credential_probe::PROBE_REPLY
    );
    assert!(probe.requests().iter().all(|request| {
        request
            .authorization()
            .is_some_and(|value| value.contains("PRX-CHILD-RETRY-CREDENTIAL"))
    }));
}

/// Provider fallbacks and root rotation keys belong to the parent chain;
/// an admitted named child keeps only its selected vendor and credential.
#[tokio::test]
async fn override_reliability_drops_chain_widening_knobs() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};

    let temp = tempfile::tempdir().expect("test: temp dir");
    let selected = CredentialProbe::start_failing(429, "selected child refused").await;
    let fallback = CredentialProbe::start().await;
    let selected_provider = selected.custom_provider();
    let fallback_provider = fallback.custom_provider();
    let agents = HashMap::from([(
        "prx_agent".to_string(),
        containment_agent(&selected_provider, Some("PRX-CHILD-ONLY-CREDENTIAL")),
    )]);
    let mut tool = containment_tool(&temp, agents, "ollama", Some("PRX-PARENT-ROOT-CREDENTIAL"));
    let mut config = preconstruction_parent_config(&tool, temp.path());
    config.reliability.provider_retries = 0;
    config.reliability.fallback_providers = vec![fallback_provider.clone()];
    config.reliability.api_keys = vec!["PRX-PARENT-ROTATION-CREDENTIAL".to_string()];
    for provider in [&selected_provider, &fallback_provider] {
        config.providers.models.push(
            serde_json::from_value(json!({
                "provider": provider, "model": PROBE_MODEL,
                "operations": ["chat"], "native_tools": true
            }))
            .expect("test: child model declaration"),
        );
        config.cost.prices.insert(
            format!("{provider}/{PROBE_MODEL}"),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
    }
    let catalog = providers::selection::catalog_for(&config);
    tool = tool
        .with_hint_config(&config)
        .with_model_catalog(catalog)
        .with_configured_providers(&providers::ConfiguredProviders::from_config(&config));
    let runs = tool.active_runs_arc();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: parent memory"));
    let result = dispatch_preconstruction_parent(
        tool,
        config,
        memory,
        with_spawn_grant(json!({"task": "probe", "agent": "prx_agent"})),
    )
    .await
    .expect("test: admitted contained spawn");
    assert!(result.success, "named child was not started: {:?}", result.error);
    assert_eq!(
        selected.wait_for_requests(1, std::time::Duration::from_secs(10)).await,
        1
    );
    let run_id = runs.read().await.first().expect("test: child run").id.clone();
    let failure = await_terminal_reason(&runs, &run_id).await;
    assert!(failure.contains("selected child refused"), "{failure}");
    let requests = selected.requests();
    assert_eq!(requests.len(), 1, "rotation credentials widened the child chain");
    assert!(requests.first().is_some_and(|request| {
        request
            .authorization()
            .is_some_and(|value| value.contains("PRX-CHILD-ONLY-CREDENTIAL"))
    }));
    assert!(!selected.saw("PRX-PARENT-ROOT-CREDENTIAL"));
    assert!(!selected.saw("PRX-PARENT-ROTATION-CREDENTIAL"));
    assert!(
        fallback.requests().is_empty(),
        "parent fallback provider received child traffic"
    );
}

/// R-PMMC F-5: every production construction of the spawn tool hands it
/// the model evidence, so operator declarations are consulted on every
/// surface (chat's `prx chat` registry included).
#[test]
fn every_production_spawn_tool_carries_the_model_catalog() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut missing = Vec::new();
    for file in [
        "chat/run.rs",
        "gateway/mod.rs",
        "channels/startup.rs",
        "session_worker/runner/provider.rs",
    ] {
        let text = std::fs::read_to_string(root.join(file)).expect("test: read source");
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let code = !line.trim_start().starts_with("//");
            if code && line.contains("SessionsSpawnTool::new") {
                let tail = &text[offset..];
                let statement = &tail[..tail.find(";\n").unwrap_or(tail.len())];
                if !statement.contains(".with_model_catalog(") {
                    missing.push(file);
                }
            }
            offset += line.len();
        }
    }
    assert!(
        missing.is_empty(),
        "spawn tools built without the model catalog in: {missing:?}"
    );
}

/// INT-2B USERINFO-3, RED before: a spawn on a `custom:` endpoint with
/// `user:password@` logged the raw label in the compaction-window debug
/// line, and when the provider could not be built the refusal quoted the
/// raw label and the unsanitized factory error. After: both are redacted.
#[tokio::test]
async fn a_spawn_never_logs_or_returns_provider_url_userinfo() {
    // Log-capturing tests share per-callsite interest process-wide.
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    #[derive(Clone, Default)]
    struct Sink(Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer({
            let sink = sink.clone();
            move || sink.clone()
        })
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let temp = tempfile::tempdir().expect("test: temp dir");
    // Port 99999 cannot be built into a provider.
    let agents = HashMap::from([(
        "broken".to_string(),
        containment_agent(
            "custom:http://u:SPAWN-SECRET-pw@127.0.0.1:99999/v1",
            Some(SPAWN_AGENT_SENTINEL),
        ),
    )]);
    let tool = containment_tool(&temp, agents, "ollama", None);
    let result = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "probe", "agent": "broken"})))
        .await
        .expect("test: spawn returns a tool result");

    assert!(!result.success, "the provider cannot be built");
    let error = result.error.unwrap_or_default();
    assert!(error.contains("[redacted]"), "{error}");
    assert!(!error.contains("SPAWN-SECRET-pw"), "{error}");
    let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
    // The value the compaction line prints, checked directly
    // (deterministic); the log capture is checked for absence only.
    let config = crate::config::Config::default();
    let catalog = providers::selection::catalog_for(&config);
    let shown = resolve_pinned_child_compaction(
        &config,
        &catalog,
        "custom:http://u:SPAWN-SECRET-pw@127.0.0.1:99999/v1",
        "m",
    )
    .shown_provider()
    .into_owned();
    assert_eq!(shown, "custom:http://[redacted]@127.0.0.1:99999/v1");
    assert!(!logs.contains("SPAWN-SECRET-pw"), "{logs}");
}

/// PMM-C: a spawn whose model cannot chat is refused before any run or
/// provider exists.
#[tokio::test]
async fn a_spawn_of_a_decision_only_model_is_refused_before_transport() {
    use crate::providers::credential_probe::CredentialProbe;
    let temp = tempfile::tempdir().expect("test: temp dir");
    let own = CredentialProbe::start().await;
    let mut decision = containment_agent(&own.custom_provider(), Some(SPAWN_AGENT_SENTINEL));
    decision.model = "jev-decision-test".to_string();
    let agents = HashMap::from([("a".to_string(), decision)]);
    let catalog = providers::catalog::CatalogView::default().with_curated([providers::catalog::CuratedModel {
        operations: Some(vec![providers::catalog::ModelOperation::DecisionEndpoint]),
        ..providers::catalog::CuratedModel::new(
            providers::ProviderId::of(&own.custom_provider()),
            Some("jev-decision-test"),
        )
    }]);
    let tool = containment_tool(&temp, agents, "ollama", None).with_model_catalog(catalog);

    let mut config = preconstruction_parent_config(&tool, temp.path());
    config.providers.models.push(
        serde_json::from_value(json!({
            "provider": own.custom_provider(), "model": "jev-decision-test", "operations": ["decision_endpoint"]
        }))
        .expect("test: original decision-only child catalog"),
    );
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: parent memory"));
    let result = dispatch_preconstruction_parent(
        tool,
        config,
        memory,
        with_spawn_grant(json!({"task": "probe", "agent": "a"})),
    )
    .await
    .expect("test: spawn returns a tool result");
    if result.success {
        own.wait_for_requests(1, std::time::Duration::from_secs(5)).await;
    }
    assert!(!result.success, "a decision-only model was spawned");
    let error = result.error.unwrap_or_default();
    assert!(error.contains("cannot be used for chat"), "{error}");
    assert!(own.requests().is_empty(), "nothing is sent");
}

/// A model-named `custom:<url>` the operator never configured is refused
/// before anything is spawned, and the named agent's key never reaches it.
#[tokio::test]
async fn a_spawn_to_an_unconfigured_endpoint_is_refused() {
    use crate::providers::credential_probe::CredentialProbe;
    let temp = tempfile::tempdir().expect("test: temp dir");
    let own = CredentialProbe::start().await;
    let foreign = CredentialProbe::start().await;
    let agents = HashMap::from([(
        "a".to_string(),
        containment_agent(&own.custom_provider(), Some(SPAWN_AGENT_SENTINEL)),
    )]);
    let tool = containment_tool(&temp, agents, "ollama", None);

    let config = preconstruction_parent_config(&tool, temp.path());
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: parent memory"));
    let result = dispatch_preconstruction_parent(
        tool,
        config,
        memory,
        with_spawn_grant(json!({"task": "probe", "agent": "a", "provider": foreign.custom_provider()})),
    )
    .await
    .expect("test: spawn returns a tool result");
    if result.success {
        foreign.wait_for_requests(1, std::time::Duration::from_secs(5)).await;
    }

    assert!(
        !foreign.saw(SPAWN_AGENT_SENTINEL),
        "agent a's key reached an unconfigured endpoint"
    );
    assert!(!result.success, "an unconfigured endpoint must be refused");
    assert!(result.error.unwrap_or_default().contains("not configured"));
    assert!(foreign.requests().is_empty());
}

/// A configured provider named per call gets only its own credential.
#[tokio::test]
async fn a_spawned_agent_key_stays_with_its_own_provider() {
    use crate::providers::credential_probe::CredentialProbe;
    let temp = tempfile::tempdir().expect("test: temp dir");
    let own = CredentialProbe::start().await;
    let other = CredentialProbe::start().await;
    let agents = HashMap::from([
        (
            "a".to_string(),
            containment_agent(&own.custom_provider(), Some(SPAWN_AGENT_SENTINEL)),
        ),
        ("b".to_string(), containment_agent(&other.custom_provider(), None)),
    ]);
    let mut tool = containment_tool(&temp, agents, &own.custom_provider(), Some(SPAWN_ROOT_SENTINEL));
    let mut root_config = preconstruction_parent_config(&tool, temp.path());
    root_config.api_key = Some(SPAWN_ROOT_SENTINEL.to_owned());
    tool = tool.with_hint_config(&root_config);

    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "task": "probe", "agent": "a", "provider": other.custom_provider()
        })),
    )
    .await
    .expect("test: spawn returns a tool result");
    assert!(result.success, "a configured endpoint is spawnable: {:?}", result.error);
    let seen = other.wait_for_requests(1, std::time::Duration::from_secs(10)).await;

    assert_eq!(seen, 1, "the spawned run reaches its provider");
    assert!(
        !other.saw(SPAWN_AGENT_SENTINEL),
        "agent a's key reached another provider"
    );
    assert!(other.requests().iter().all(|request| request.authorization().is_none()));
    assert!(
        !other.saw(SPAWN_ROOT_SENTINEL),
        "root key reached an independently selected provider"
    );
    assert!(own.requests().is_empty());
}

/// Guard: the gateway's own key still reaches the gateway provider when a
/// run names it in another spelling. Since C0-f that spelling is served by
/// the gateway chain itself, so the chain is a real one here.
#[tokio::test]
async fn the_gateway_key_still_reaches_the_gateway_provider() {
    use crate::providers::credential_probe::CredentialProbe;
    let temp = tempfile::tempdir().expect("test: temp dir");
    let gateway = CredentialProbe::start().await;
    let mut tool = containment_tool(
        &temp,
        HashMap::new(),
        &gateway.custom_provider(),
        Some(SPAWN_ROOT_SENTINEL),
    );
    let gateway_config = crate::config::Config {
        default_provider: Some(gateway.custom_provider()),
        default_model: Some(crate::providers::credential_probe::PROBE_MODEL.to_string()),
        api_key: Some(SPAWN_ROOT_SENTINEL.to_string()),
        ..crate::config::Config::default()
    };
    tool.test_parent_provider = Some(Arc::from(
        providers::create_provider_from_test_config(&gateway_config, &providers::ProviderRuntimeOptions::default())
            .expect("test: gateway chain builds"),
    ));
    tool = tool.with_hint_config(&gateway_config);

    let result = execute_enabled_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "task": "sessions_spawn: probe", "provider": format!("{}/", gateway.custom_provider())
        })),
    )
    .await
    .expect("test: spawn returns a tool result");
    assert!(result.success, "{:?}", result.error);
    gateway.wait_for_requests(1, std::time::Duration::from_secs(10)).await;

    assert!(gateway.saw(SPAWN_ROOT_SENTINEL));
}

// ── C0-f F-1: the root key travels only with the root URL ─────────────

const F1_SPAWN_ROOT_SENTINEL: &str = "C0F-SPAWN-ROOT-SENTINEL";

/// A routed parent keeps its route key for an inherited child, while a
/// direct-model child returns to the configured root connection.
#[tokio::test]
async fn child_provider_root_branch_uses_resolved_root_credential_on_wire() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
    let temp = tempfile::tempdir().expect("test: temp dir");
    let root = CredentialProbe::start().await;
    let route = CredentialProbe::start().await;
    let root_provider = root.custom_provider();
    let route_provider = route.custom_provider();
    let mut config = crate::config::Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some(root_provider.clone());
    config.default_model = Some("hint:fast".to_string());
    config.api_key = Some("PRX-HINT-ROOT-WIRE".to_string());
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: route_provider.clone(),
        model: PROBE_MODEL.to_string(),
        api_key: Some("PRX-HINT-ROUTE-WIRE".to_string()),
        credential_ref: None,
    });
    config.cost.prices.insert(
        format!("{root_provider}/{PROBE_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let catalog = providers::selection::catalog_for(&config);
    let selected = providers::selection::check_default(&config, providers::catalog::UnknownPolicy::Invocation)
        .expect("test: default hint resolves");
    let routed_provider = providers::create_provider_for(
        &selected,
        &catalog,
        &config.reliability,
        &config.cost,
        &providers::ProviderRuntimeOptions::default(),
    )
    .expect("test: routed provider builds");
    let mut tool = containment_tool(&temp, HashMap::new(), &root_provider, Some("PRX-HINT-ROOT-WIRE"))
        .with_hint_config(&config)
        .with_model_catalog(catalog)
        .with_configured_providers(&providers::ConfiguredProviders::from_config(&config))
        .with_cost_config(config.cost.clone());
    tool.model = "hint:fast".to_string();
    tool.test_parent_provider = Some(Arc::from(routed_provider));
    let inherited = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "probe"})))
        .await
        .expect("test: inherited spawn result");
    assert!(inherited.success, "{:?}", inherited.error);
    assert_eq!(route.wait_for_requests(1, std::time::Duration::from_secs(10)).await, 1);
    assert!(route.saw("PRX-HINT-ROUTE-WIRE"));
    assert!(!route.saw("PRX-HINT-ROOT-WIRE"));
    assert!(root.requests().is_empty());

    let direct = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "probe", "model": PROBE_MODEL})))
        .await
        .expect("test: direct override spawn result");
    assert!(direct.success, "{:?}", direct.error);
    assert_eq!(root.wait_for_requests(1, std::time::Duration::from_secs(10)).await, 1);
    assert!(root.saw("PRX-HINT-ROOT-WIRE"));
    assert!(!root.saw("PRX-HINT-ROUTE-WIRE"));
    assert_eq!(route.requests().len(), 1);
}

/// A named direct agent on the hinted parent's source provider owns a
/// separate key. Task mode must select it just as process mode does.
#[tokio::test]
async fn child_provider_build_branch_uses_named_agent_credential_on_wire() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
    let temp = tempfile::tempdir().expect("test: temp dir");
    let root = CredentialProbe::start().await;
    let route = CredentialProbe::start().await;
    let root_provider = root.custom_provider();
    let mut config = crate::config::Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some(root_provider.clone());
    config.default_model = Some("hint:fast".to_string());
    config.api_key = Some("PRX-HINT-ROOT-WIRE".to_string());
    config.secrets.encrypt = false;
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: route.custom_provider(),
        model: PROBE_MODEL.to_string(),
        api_key: Some("PRX-HINT-ROUTE-WIRE".to_string()),
        credential_ref: None,
    });
    let catalog = providers::selection::catalog_for(&config);
    let selected = providers::selection::check_default(&config, providers::catalog::UnknownPolicy::Invocation)
        .expect("test: default hint resolves");
    let routed_provider = providers::create_provider_for(
        &selected,
        &catalog,
        &config.reliability,
        &config.cost,
        &providers::ProviderRuntimeOptions::default(),
    )
    .expect("test: routed provider builds");
    let profile = crate::config::manage::credentials::CredentialReferences::for_config(&config)
        .expect("isolated credential store")
        .set(&root_provider, "prx-direct-agent", "PRX-HINT-PROFILE-WIRE")
        .expect("store direct agent profile");
    let mut profile_agent = containment_agent(&root_provider, None);
    profile_agent.credential_ref = Some(profile.id.clone());
    config.agents.insert("prx_profile".to_string(), profile_agent);
    let inline_agent = containment_agent(&root_provider, Some("PRX-HINT-AGENT-WIRE"));
    let agents = HashMap::from([
        ("prx_agent".to_string(), inline_agent.clone()),
        (
            "prx_profile".to_string(),
            config
                .agents_with_references()
                .get("prx_profile")
                .expect("resolved agent")
                .clone(),
        ),
    ]);
    let mut tool = containment_tool(&temp, agents, &root_provider, Some("PRX-HINT-ROOT-WIRE"))
        .with_hint_config(&config)
        .with_model_catalog(catalog)
        .with_configured_providers(&providers::ConfiguredProviders::from_config(&config));
    tool.model = "hint:fast".to_string();
    tool.test_parent_provider = Some(Arc::from(routed_provider));
    let result = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "probe", "agent": "prx_agent"})))
        .await
        .expect("test: direct agent child result");
    assert!(result.success, "{:?}", result.error);
    assert_eq!(root.wait_for_requests(1, std::time::Duration::from_secs(10)).await, 1);
    assert!(root.saw("PRX-HINT-AGENT-WIRE"));
    assert!(!root.saw("PRX-HINT-ROOT-WIRE"));
    assert!(!root.saw("PRX-HINT-ROUTE-WIRE"));
    assert!(route.requests().is_empty());

    let profile_result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({"task": "probe", "agent": "prx_profile"})),
    )
    .await
    .expect("test: profile agent child result");
    assert!(profile_result.success, "{:?}", profile_result.error);
    assert_eq!(root.wait_for_requests(2, std::time::Duration::from_secs(10)).await, 2);
    let requests = root.requests();
    let profile_request = requests.get(1).expect("profile agent request");
    assert!(profile_request.carries("PRX-HINT-PROFILE-WIRE"));
    assert!(!profile_request.carries("PRX-HINT-ROOT-WIRE"));
    assert!(!profile_request.carries("PRX-HINT-AGENT-WIRE"));

    crate::config::manage::credentials::CredentialReferences::for_config(&config)
        .expect("isolated credential store")
        .remove(&profile.id)
        .expect("remove isolated credential");
    let missing = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({"task": "probe", "agent": "prx_profile"})),
    )
    .await
    .expect("test: missing profile spawn result");
    assert!(!missing.success, "missing profile must be refused");
    assert_eq!(root.requests().len(), 2, "missing profile caused outbound I/O");
}

/// On the wire: the respelled child is served by the gateway chain, whose
/// connection is the root key at the root URL.
#[tokio::test]
async fn f1_wire_a_respelled_gateway_child_reaches_only_the_root_url() {
    use crate::providers::credential_probe::CredentialProbe;
    let temp = tempfile::tempdir().expect("test: temp dir");
    let root = CredentialProbe::start().await;
    let gateway_config = crate::config::Config {
        default_provider: Some("litellm".to_string()),
        default_model: Some("probe-model".to_string()),
        api_key: Some(F1_SPAWN_ROOT_SENTINEL.to_string()),
        api_url: Some(root.base_url()),
        reliability: crate::config::ReliabilityConfig {
            provider_retries: 0,
            provider_backoff_ms: 0,
            ..crate::config::ReliabilityConfig::default()
        },
        workspace_dir: temp.path().join("workspace"),
        ..crate::config::Config::default()
    };
    let gateway = providers::create_provider_from_test_config(
        &gateway_config,
        &providers::ProviderRuntimeOptions {
            openprx_dir: Some(temp.path().to_path_buf()),
            codex_auth_json_path: Some(temp.path().join("no-codex-auth.json")),
            codex_auth_json_auto_import: false,
            ..providers::ProviderRuntimeOptions::default()
        },
    )
    .expect("test: litellm gateway builds");
    let mut tool = containment_tool(&temp, HashMap::new(), "litellm", Some(F1_SPAWN_ROOT_SENTINEL))
        .with_hint_config(&gateway_config);
    tool.test_parent_provider = Some(Arc::from(gateway));

    for (index, spelling) in ["litellm", "lite-llm", "LiteLLM"].into_iter().enumerate() {
        let result = execute_enabled_admitted_spawn(
            &tool,
            with_spawn_grant(json!({"task": "sessions_spawn: probe", "provider": spelling})),
        )
        .await
        .expect("test: spawn returns a tool result");
        assert!(result.success, "{spelling}: {:?}", result.error);
        let seen = root
            .wait_for_requests(index + 1, std::time::Duration::from_secs(10))
            .await;
        let runs = tool.active_runs_snapshot().await;
        assert_eq!(seen, index + 1, "gateway child did not reach root endpoint: {runs:?}");
    }
    assert!(root.saw(F1_SPAWN_ROOT_SENTINEL));
    assert!(root.requests().iter().all(|request| {
        request
            .authorization()
            .is_some_and(|header| header.contains(F1_SPAWN_ROOT_SENTINEL))
    }));
}
