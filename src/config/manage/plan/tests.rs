use super::*;
use crate::config::secret_paths::fixture;

fn loaded_fixture(dir: &std::path::Path) -> Config {
    fixture::config_in(dir)
}

/// Guard (D-1): a plan over a configuration that holds a sentinel at
/// every registered secret path renders no sentinel, while every one of
/// those paths is still reported as a changed secret.
#[test]
fn a_plan_over_every_secret_path_renders_no_secret() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let with_secrets = loaded_fixture(dir.path());
    let mut empty = Config::default();
    empty.config_path = with_secrets.config_path.clone();
    empty.workspace_dir = with_secrets.workspace_dir.clone();

    for summary in [
        ConfigChangeSummary::between(Some(&empty), &with_secrets).expect("test: add secrets"),
        ConfigChangeSummary::between(Some(&with_secrets), &empty).expect("test: remove secrets"),
        ConfigChangeSummary::between(None, &with_secrets).expect("test: no prior config"),
    ] {
        let rendered = serde_json::to_string(&summary).expect("test: render summary");
        assert!(
            !rendered.contains(fixture::SENTINEL_PREFIX),
            "a plan leaked a secret: {rendered}"
        );
        assert_eq!(summary.risk, ConfigChangeRisk::High);
        let mut secret_paths = summary
            .changes
            .iter()
            .filter(|change| change.secret)
            .map(|change| change.path.clone())
            .collect::<Vec<_>>();
        secret_paths.sort();
        let sentinel_paths =
            fixture::string_leaves(&serde_json::to_value(&with_secrets).expect("test: json"), &|value| {
                value.starts_with(fixture::SENTINEL_PREFIX)
            });
        assert!(!sentinel_paths.is_empty(), "test precondition: sentinels present");
        for path in sentinel_paths.iter().map(|path| secret_paths::display_path(path)) {
            assert!(
                secret_paths.contains(&path),
                "the plan does not report the secret at {path}: {secret_paths:?}"
            );
        }
    }
}

#[test]
fn an_unchanged_configuration_has_an_empty_low_risk_plan() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded_fixture(dir.path());
    let summary = ConfigChangeSummary::between(Some(&config), &config).expect("test: summary");
    assert!(summary.is_empty(), "{summary:?}");
    assert_eq!(summary.risk, ConfigChangeRisk::Low);
}

#[test]
fn a_plain_setting_shows_its_values_and_is_low_risk() {
    let before = Config::default();
    let mut after = before.clone();
    after.default_temperature = 0.25;
    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::Low);
    assert_eq!(
        summary.changes,
        vec![ConfigChangeEntry {
            path: "default_temperature".into(),
            kind: ChangeKind::Changed,
            before: Some(serde_json::json!(before.default_temperature)),
            after: Some(serde_json::json!(0.25)),
            secret: false,
        }]
    );
}

/// CDP-04 (SP-CDP-A 5.4): turning remote decision processing on, or
/// changing where it goes while it is on, is high risk through the one
/// classifier; the decision endpoint is a credential recipient; a
/// credential reference change is a credential change; narrowing stays
/// low.
#[test]
fn remote_decision_processing_changes_are_high_risk_and_narrowing_is_low() {
    use crate::config::schema::decision_engine::BackendKind;
    use crate::config::schema::decision_engine::{BackendSelection, CredentialRefSetting};
    let reasons = |before: &Config, after: &Config| {
        let summary = ConfigChangeSummary::between(Some(before), after).expect("test: summary");
        assert_eq!(
            summary.risk,
            if summary.risk_reasons.is_empty() {
                ConfigChangeRisk::Low
            } else {
                ConfigChangeRisk::High
            }
        );
        summary.risk_reasons
    };
    let remote = |field: &str| RiskReason::RemoteDecisionEnabled {
        path: format!("decision_engine.backend.{field}"),
    };
    let local = Config::default();
    let mut remote_on = local.clone();
    remote_on.decision_engine.enabled = true;
    remote_on
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
    remote_on
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .endpoint = "https://decisions.example.com/v1".into();
    remote_on
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .remote_enabled = true;
    let turned_on = reasons(&local, &remote_on);
    assert!(turned_on.contains(&remote("remote_enabled")), "{turned_on:?}");
    assert!(
        turned_on.contains(&RiskReason::EndpointChanged {
            path: "decision_engine.backend.endpoint".into()
        }),
        "{turned_on:?}"
    );
    assert!(
        ConfigChangeSummary::between(None, &remote_on)
            .expect("test: first write")
            .risk_reasons
            .contains(&remote("remote_enabled"))
    );

    for (field, change) in [
        (
            "model",
            (|config: &mut Config| {
                config
                    .decision_engine
                    .backend
                    .get_or_insert_with(Default::default)
                    .model = "decider-2".into()
            }) as fn(&mut Config),
        ),
        ("kind", |config| {
            config.decision_engine.backend.get_or_insert_with(Default::default).kind = BackendSelection::Auto
        }),
        ("endpoint", |config| {
            config
                .decision_engine
                .backend
                .get_or_insert_with(Default::default)
                .endpoint = "https://other.example.com/v1".into();
        }),
        ("credential_ref", |config| {
            config
                .decision_engine
                .backend
                .get_or_insert_with(Default::default)
                .credential_ref = CredentialRefSetting::None
        }),
    ] {
        let mut after = remote_on.clone();
        change(&mut after);
        let found = reasons(&remote_on, &after);
        assert!(found.contains(&remote(field)), "{field}: {found:?}");
    }

    // Narrowing: remote off, and builtin, are low.
    let mut off = remote_on.clone();
    off.decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .remote_enabled = false;
    assert_eq!(reasons(&remote_on, &off), Vec::new());
    // D-079 M-2: with remote on, a move to the builtin, and a change on a
    // local target, send nothing off the machine and stay low.
    let mut to_builtin = remote_on.clone();
    to_builtin
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .kind = BackendSelection::Fixed(BackendKind::Builtin);
    assert_eq!(
        reasons(&remote_on, &to_builtin),
        Vec::new(),
        "kind -> builtin is a narrowing"
    );
    let mut local_remote_on = local.clone();
    local_remote_on.decision_engine.enabled = true;
    local_remote_on
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
    local_remote_on
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .endpoint = "http://127.0.0.1:8900/v1".into();
    local_remote_on
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .remote_enabled = true;
    let mut local_model = local_remote_on.clone();
    local_model
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .model = "decider-3".into();
    assert_eq!(
        reasons(&local_remote_on, &local_model),
        Vec::new(),
        "a local model change is low"
    );
    // D-082 N-5: a remote target moved to a loopback endpoint sends
    // nothing off the machine; only the re-pointed credential recipient
    // is high.
    let mut to_loopback = remote_on.clone();
    to_loopback
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .endpoint = "http://127.0.0.1:8900/v1".into();
    assert_eq!(
        reasons(&remote_on, &to_loopback),
        vec![RiskReason::EndpointChanged {
            path: "decision_engine.backend.endpoint".into()
        }]
    );
    // A local target that becomes remote while the switch is on is high.
    let mut becomes_remote = local_remote_on.clone();
    becomes_remote
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .endpoint = "https://decisions.example.com/v1".into();
    assert!(reasons(&local_remote_on, &becomes_remote).contains(&remote("endpoint")));
    // D-079 L-1: enabling the engine with a remote target configured is
    // high; disabling it is low.
    let mut disabled = remote_on.clone();
    disabled.decision_engine.enabled = false;
    assert_eq!(reasons(&remote_on, &disabled), Vec::new());
    assert_eq!(
        reasons(&disabled, &remote_on),
        vec![RiskReason::RemoteDecisionEnabled {
            path: "decision_engine.enabled".into()
        }]
    );
    let mut builtin = local.clone();
    builtin
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .kind = BackendSelection::Fixed(BackendKind::Builtin);
    assert_eq!(reasons(&local, &builtin), Vec::new());
    let mut enabled = local.clone();
    enabled.decision_engine.enabled = true;
    assert_eq!(
        reasons(&local, &enabled),
        Vec::new(),
        "the local decision engine is low"
    );

    // With remote off: a new endpoint re-points the backend's credential,
    // and a credential reference change is a credential change.
    let mut endpoint = local.clone();
    endpoint
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .endpoint = "http://127.0.0.1:8900/v1".into();
    assert_eq!(
        reasons(&local, &endpoint),
        vec![RiskReason::EndpointChanged {
            path: "decision_engine.backend.endpoint".into()
        }]
    );
    let mut credential = local.clone();
    credential
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .credential_ref = CredentialRefSetting::None;
    assert_eq!(
        reasons(&local, &credential),
        vec![RiskReason::CredentialChanged {
            path: "decision_engine.backend.credential_ref".into()
        }]
    );
}

#[test]
fn disabling_encryption_and_redirecting_an_endpoint_are_high_risk() {
    let before = Config::default();
    let mut after = before.clone();
    after.secrets.encrypt = false;
    after.api_url = Some("https://elsewhere.example/v1".into());
    after.default_provider = Some("custom:https://proxy.example/v1".into());
    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::High);
    assert!(summary.risk_reasons.contains(&RiskReason::EncryptionDisabled));
    assert!(
        summary
            .risk_reasons
            .contains(&RiskReason::EndpointChanged { path: "api_url".into() })
    );
    assert!(summary.risk_reasons.contains(&RiskReason::EndpointChanged {
        path: "default_provider".into()
    }));
}

#[test]
fn an_added_route_reports_each_leaf_and_masks_its_key() {
    let before = Config::default();
    let mut after = before.clone();
    after.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".into(),
        provider: "openrouter".into(),
        model: "m".into(),
        api_key: Some("sentinel-added-route".into()),
        credential_ref: None,
    });
    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    let key = summary
        .changes
        .iter()
        .find(|change| change.path == "model_routes[0].api_key")
        .expect("test: route key reported");
    assert_eq!(key.kind, ChangeKind::Added);
    assert!(key.secret);
    assert_eq!(key.after, Some(serde_json::json!(secret_paths::REDACTION_MASK)));
    let hint = summary
        .changes
        .iter()
        .find(|change| change.path == "model_routes[0].hint")
        .expect("test: route hint reported");
    assert_eq!(hint.after, Some(serde_json::json!("fast")));
    assert!(summary.risk_reasons.contains(&RiskReason::CredentialChanged {
        path: "model_routes[0].api_key".into()
    }));
}

/// PMMD-g (N-3): URL userinfo in provider and endpoint values and in a
/// `cost.prices` key never reaches a plan path or value, and each such
/// change is a credential change.
#[test]
fn a_plan_over_url_userinfo_renders_no_userinfo() {
    const PW: &str = "userinfo-sentinel-pw";
    let url = format!("custom:http://alice:{PW}@127.0.0.1:1/v1");
    let before = Config::default();
    let mut after = before.clone();
    after.default_provider = Some(url.clone());
    after.api_url = Some(format!("http://bob:{PW}@127.0.0.1:2/v1"));
    after.reliability.fallback_providers = vec![url.clone()];
    after.cost.prices.insert(
        format!("{url}/model-a"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 2.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );

    for summary in [
        ConfigChangeSummary::between(Some(&before), &after).expect("test: add"),
        ConfigChangeSummary::between(Some(&after), &before).expect("test: remove"),
    ] {
        let rendered = serde_json::to_string(&summary).expect("test: json");
        assert!(!rendered.contains(PW), "a plan leaked userinfo: {rendered}");
        assert_eq!(summary.risk, ConfigChangeRisk::High);
        for path in [
            "default_provider",
            "api_url",
            "reliability.fallback_providers[0]",
            "cost.prices.custom:http://[redacted]@127.0.0.1:1/v1/model-a.input",
        ] {
            assert!(
                summary
                    .risk_reasons
                    .contains(&RiskReason::CredentialChanged { path: path.into() }),
                "{path}: {:?}",
                summary.risk_reasons
            );
        }
    }
    let price = ConfigChangeSummary::between(Some(&before), &after)
        .expect("test: add")
        .changes
        .into_iter()
        .find(|change| change.path.ends_with("model-a.input"))
        .expect("test: price change");
    assert_eq!(
        price.after,
        Some(serde_json::json!(1.0)),
        "the value is shown, the key redacted"
    );
}

/// RED before PMMD-h (G-3): re-pointing an endpoint that receives a
/// stored token (MCP server, remote node, channel, telemetry, chat
/// daemon) classified low.
#[test]
fn token_receiving_endpoints_are_high_risk() {
    let (old, new) = (
        serde_json::json!("https://a.example"),
        serde_json::json!("https://b.example"),
    );
    let key = |name: &str| PathElem::Key(name.to_string());
    for path in [
        vec![key("mcp"), key("servers"), key("remote"), key("url")],
        vec![key("nodes"), key("nodes"), PathElem::Index(0), key("endpoint")],
        vec![key("channels_config"), key("mattermost"), key("url")],
        vec![key("channels_config"), key("signal"), key("http_url")],
        vec![key("channels_config"), key("whatsapp"), key("ws_url")],
        vec![key("observability"), key("otel_endpoint")],
        vec![key("chat"), key("daemon"), key("url")],
        vec![key("proxy"), key("https_proxy")],
        vec![key("channels_config"), key("irc"), key("server")],
    ] {
        assert!(
            secret_paths::is_credential_recipient_endpoint(&path)
                || is_endpoint_change(&path, secret_paths::is_url_userinfo_path(&path), Some(&old), Some(&new)),
            "{}",
            secret_paths::display_path(&path)
        );
    }

    let before = Config::default();
    let mut after = before.clone();
    after.observability.otel_endpoint = Some("https://collector.example:4318".into());
    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");

    // A first write (no prior configuration) re-points nothing: default
    // token endpoints coming into existence are not endpoint changes.
    let first = ConfigChangeSummary::between(None, &Config::default()).expect("test: first write");
    assert!(
        !first
            .risk_reasons
            .iter()
            .any(|reason| matches!(reason, RiskReason::EndpointChanged { .. })),
        "{:?}",
        first.risk_reasons
    );
}

/// RED before PMMD-i (H-2): a stdio MCP server receives its stored `env`
/// credentials; pointing its `command` or `args` at another executable
/// hands them over and was classified low.
#[test]
fn relaunching_an_mcp_server_that_holds_credentials_is_high_risk() {
    let mut before = Config::default();
    before.mcp.servers.insert(
        "local".into(),
        serde_json::from_value(serde_json::json!({
            "command": "mcp-local",
            "args": ["--serve"],
            "env": { "MCP_TOKEN": "sentinel-mcp-env" }
        }))
        .expect("test: server"),
    );
    let relaunch: [fn(&mut crate::config::McpServerConfig); 2] = [
        |server| server.command = Some("/tmp/other-binary".into()),
        |server| server.args = vec!["--exfiltrate".into()],
    ];
    for change in relaunch {
        let mut after = before.clone();
        change(after.mcp.servers.get_mut("local").expect("test: server"));
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
        assert!(
            !serde_json::to_string(&summary)
                .expect("test: json")
                .contains("sentinel-")
        );
    }
}

/// RED before PMME-f (D-085 F-3): switching the default provider to
/// another catalog provider while the root `api_key` is set hands that
/// key to the new vendor; it was low risk on every write path. The same
/// for a route or an agent that moves its own key to another provider.
#[test]
fn handing_a_stored_key_to_another_provider_is_high_risk() {
    let mut before = Config::default();
    before.default_provider = Some("openai".into());
    before.api_key = Some("sentinel-root-key".into());
    before.model_routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".into(),
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: Some("sentinel-route-key".into()),
        credential_ref: None,
    }];
    let owner_changes = |summary: &ConfigChangeSummary| {
        summary
            .risk_reasons
            .iter()
            .filter_map(|reason| match reason {
                RiskReason::CredentialOwnerChanged { path } => Some(path.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    let mut switched = before.clone();
    switched.default_provider = Some("anthropic".into());
    if let Some(route) = switched.model_routes.first_mut() {
        route.provider = "openrouter".into();
    }
    let summary = ConfigChangeSummary::between(Some(&before), &switched).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
    assert_eq!(
        owner_changes(&summary),
        ["default_provider", "model_routes[0].provider"]
    );

    // A respelling of the same provider, or a switch with no key held,
    // hands nothing over.
    let mut respelled = before.clone();
    respelled.default_provider = Some("OpenAI".into());
    let summary = ConfigChangeSummary::between(Some(&before), &respelled).expect("test: summary");
    assert!(owner_changes(&summary).is_empty(), "{summary:?}");
    let mut keyless = before;
    keyless.api_key = None;
    keyless.model_routes.clear();
    let mut keyless_switch = keyless.clone();
    keyless_switch.default_provider = Some("anthropic".into());
    let summary = ConfigChangeSummary::between(Some(&keyless), &keyless_switch).expect("test: summary");
    assert!(owner_changes(&summary).is_empty(), "{summary:?}");
    // `reliability.api_keys` is root material too.
    let mut rotated = keyless.clone();
    rotated.reliability.api_keys = vec!["sentinel-rotation".into()];
    let mut rotated_switch = rotated.clone();
    rotated_switch.default_provider = Some("anthropic".into());
    let summary = ConfigChangeSummary::between(Some(&rotated), &rotated_switch).expect("test: summary");
    assert_eq!(owner_changes(&summary), ["default_provider"]);
}

/// RED before PMME-g (D-088 C-1): the owner check matched routes by hint,
/// so renaming a route's hint while moving it to another provider (key
/// kept) handed the key over at low risk. The check now follows the key.
#[test]
fn a_key_moved_under_a_renamed_route_or_embedding_route_is_high_risk() {
    let route = |hint: &str, provider: &str| crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: hint.into(),
        provider: provider.into(),
        model: "some-model".into(),
        api_key: Some("sentinel-moving-route-key".into()),
        credential_ref: None,
    };
    let mut before = Config::default();
    before.model_routes = vec![route("fast", "openai")];
    let mut after = before.clone();
    after.model_routes = vec![route("fast2", "anthropic")];
    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    assert!(
        summary.risk_reasons.contains(&RiskReason::CredentialOwnerChanged {
            path: "model_routes[0].provider".into()
        }),
        "{summary:?}"
    );
    assert!(
        !serde_json::to_string(&summary)
            .expect("test: json")
            .contains("sentinel-")
    );

    let embedding = |hint: &str, provider: &str| crate::config::EmbeddingRouteConfig {
        hint: hint.into(),
        provider: provider.into(),
        model: "text-embedding-3-small".into(),
        dimensions: None,
        api_key: Some("sentinel-moving-embedding-key".into()),
        credential_ref: None,
    };
    let mut before = Config::default();
    before.embedding_routes = vec![embedding("semantic", "openai")];
    let mut after = before.clone();
    after.embedding_routes = vec![embedding("archive", "custom:https://embed.example/v1")];
    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    assert!(
        summary.risk_reasons.contains(&RiskReason::CredentialOwnerChanged {
            path: "embedding_routes[0].provider".into()
        }),
        "{summary:?}"
    );

    // A key that stays with its provider while the hint is renamed, or
    // an index shift, moves nothing.
    let mut before = Config::default();
    before.model_routes = vec![route("fast", "openai")];
    let mut renamed = before.clone();
    renamed.model_routes = vec![route("other", "openai"), route("fast", "openai")];
    let summary = ConfigChangeSummary::between(Some(&before), &renamed).expect("test: summary");
    assert!(
        !summary
            .risk_reasons
            .iter()
            .any(|reason| matches!(reason, RiskReason::CredentialOwnerChanged { .. })),
        "{summary:?}"
    );
}

/// RED before PMME-g (D-088 C-6): a plan (and so the history record)
/// showed a provider URL's query values and fragment verbatim.
#[test]
fn a_plan_redacts_url_query_values_and_fragments() {
    let before = Config::default();
    let mut after = before.clone();
    after.api_url = Some("https://llm.example/v1?sig=sentinel-plan-sig#sentinel-plan-fragment".into());
    after.reliability.fallback_providers = vec!["custom:https://h.example/v1?sentinel-bare-token".into()];
    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    let rendered = serde_json::to_string(&summary).expect("test: json");
    assert!(!rendered.contains("sentinel-"), "{rendered}");
    assert!(
        rendered.contains("https://llm.example/v1?sig=[redacted]#[redacted]"),
        "{rendered}"
    );
}

/// RED before HOOKS-ENV: `security.workspace_hooks` did not exist, and
/// workspace hooks ran without any operator consent. Turning them on is
/// now high risk; turning them off is not.
fn route(hint: &str, api_key: Option<&str>) -> crate::config::ModelRouteConfig {
    crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: hint.into(),
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: api_key.map(str::to_string),
        credential_ref: None,
    }
}

/// RED before PMMH-f (D-101 F-3): secrets were compared by position, so
/// removing a route in front of one that holds a key read as that key
/// moving (a credential write, High). They are compared by identity now:
/// no secret changed, and the plan says so; writing a key is a write,
/// clearing one a clear.
#[test]
fn secrets_are_compared_by_identity_not_position() {
    let mut before = Config::default();
    before.model_routes = vec![route("a", None), route("b", Some("pmmh-identity-sentinel"))];
    let mut after = before.clone();
    after.model_routes.remove(0);

    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    assert!(summary.secret_writes.is_empty(), "{:?}", summary.secret_writes);
    assert!(summary.secret_clears.is_empty(), "{:?}", summary.secret_clears);
    assert!(
        !summary
            .risk_reasons
            .iter()
            .any(|reason| matches!(reason, RiskReason::CredentialChanged { .. })),
        "{:?}",
        summary.risk_reasons
    );

    let mut rotated = after.clone();
    if let Some(route) = rotated.model_routes.first_mut() {
        route.api_key = Some("pmmh-identity-rotated".into());
    }
    let summary = ConfigChangeSummary::between(Some(&after), &rotated).expect("test: summary");
    assert_eq!(summary.secret_writes, ["model_routes[0].api_key"]);
    assert!(summary.secret_clears.is_empty(), "expected an empty value");

    let mut cleared = after.clone();
    if let Some(route) = cleared.model_routes.first_mut() {
        route.api_key = None;
    }
    let summary = ConfigChangeSummary::between(Some(&after), &cleared).expect("test: summary");
    assert!(summary.secret_writes.is_empty(), "expected an empty value");
    assert_eq!(summary.secret_clears, ["model_routes[0].api_key"]);

    // RED before PMMH-g (D-104 C-1): two embedding routes sharing a
    // hint shared one identity, so changing the first one's key was no
    // secret change at all.
    let embedding = |api_key: &str| crate::config::EmbeddingRouteConfig {
        hint: "semantic".into(),
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: None,
        api_key: Some(api_key.into()),
        credential_ref: None,
    };
    let mut duplicated = Config::default();
    duplicated.embedding_routes = vec![embedding("pmmh-dup-one"), embedding("pmmh-dup-two")];
    let mut changed = duplicated.clone();
    if let Some(route) = changed.embedding_routes.first_mut() {
        route.api_key = Some("pmmh-dup-rotated".into());
    }
    let summary = ConfigChangeSummary::between(Some(&duplicated), &changed).expect("test: summary");
    assert_eq!(summary.secret_writes, ["embedding_routes[0].api_key"]);
    assert_eq!(summary.risk, ConfigChangeRisk::High);

    // A list of secrets compares as a set: reordering changes nothing.
    let mut keyed = Config::default();
    keyed.reliability.api_keys = vec!["pmmh-key-one".into(), "pmmh-key-two".into()];
    let mut reordered = keyed.clone();
    reordered.reliability.api_keys.reverse();
    let summary = ConfigChangeSummary::between(Some(&keyed), &reordered).expect("test: summary");
    assert!(summary.secret_writes.is_empty() && summary.secret_clears.is_empty());
}

/// D-085 F-1: letting a workspace `mcp.json` add stdio servers is high
/// risk; narrowing it back is not.
#[test]
fn widening_the_workspace_mcp_opt_in_is_high_risk() {
    use crate::config::WorkspaceMcpServers;
    let before = Config::default();
    let mut stdio = before.clone();
    stdio.mcp.workspace_servers = WorkspaceMcpServers::Stdio;
    let summary = ConfigChangeSummary::between(Some(&before), &stdio).expect("test: summary");
    assert!(
        summary.risk_reasons.contains(&RiskReason::CredentialRecipientChanged {
            path: "mcp.workspace_servers".into()
        }),
        "{summary:?}"
    );
    let mut http = before.clone();
    http.mcp.workspace_servers = WorkspaceMcpServers::HttpOnly;
    let summary = ConfigChangeSummary::between(Some(&before), &http).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
    let summary = ConfigChangeSummary::between(Some(&stdio), &before).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::Low, "{summary:?}");
}

/// RED before PMM-E (D-045): a stdio MCP server runs with PRX's
/// privileges and can read every stored credential, so adding one or
/// re-pointing its launch is high risk even when it holds no `env`
/// value. Before, such a change was classified low.
#[test]
fn adding_or_relaunching_a_stdio_server_without_env_is_high_risk() {
    let server = |value: serde_json::Value| -> crate::config::McpServerConfig {
        serde_json::from_value(value).expect("test: server")
    };
    let mut bare = Config::default();
    bare.mcp.servers.insert(
        "local".into(),
        server(serde_json::json!({ "command": "mcp-local", "args": ["--serve"] })),
    );
    let relaunch: [fn(&mut crate::config::McpServerConfig); 3] = [
        |server| server.command = Some("/tmp/other-binary".into()),
        |server| server.args = vec!["--exfiltrate".into()],
        |server| server.args.push("--extra".into()),
    ];
    for change in relaunch {
        let mut after = bare.clone();
        change(after.mcp.servers.get_mut("local").expect("test: server"));
        let summary = ConfigChangeSummary::between(Some(&bare), &after).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
        assert!(
            summary
                .risk_reasons
                .iter()
                .any(|reason| matches!(reason, RiskReason::CredentialRecipientChanged { .. })),
            "{summary:?}"
        );
    }

    // Adding a stdio server without env.
    let mut added = bare.clone();
    added.mcp.servers.insert(
        "fresh".into(),
        server(serde_json::json!({ "command": "npx", "args": ["some-mcp"] })),
    );
    let summary = ConfigChangeSummary::between(Some(&bare), &added).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");

    // Switching an HTTP server to stdio.
    let mut http = bare.clone();
    http.mcp.servers.insert(
        "remote".into(),
        server(serde_json::json!({ "transport": "http", "url": "https://mcp.example.com/mcp" })),
    );
    let mut switched = http.clone();
    let remote = switched.mcp.servers.get_mut("remote").expect("test: server");
    remote.transport = crate::config::McpTransport::Stdio;
    remote.command = Some("mcp-remote-local".into());
    let summary = ConfigChangeSummary::between(Some(&http), &switched).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");

    // What does not change the program stays low: a timeout, a tool
    // filter. Adding an HTTP server launches nothing (its URL is an
    // endpoint, classified as such, not a launch).
    let mut tuned = bare.clone();
    let local = tuned.mcp.servers.get_mut("local").expect("test: server");
    local.startup_timeout_ms = 20_000;
    local.deny_tools = vec!["dangerous".into()];
    let summary = ConfigChangeSummary::between(Some(&bare), &tuned).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::Low, "{summary:?}");
    let mut with_http = bare.clone();
    with_http.mcp.servers.insert(
        "remote".into(),
        server(serde_json::json!({ "transport": "http", "url": "https://mcp.example.com/mcp" })),
    );
    let summary = ConfigChangeSummary::between(Some(&bare), &with_http).expect("test: summary");
    assert!(
        !summary
            .risk_reasons
            .iter()
            .any(|reason| matches!(reason, RiskReason::CredentialRecipientChanged { .. })),
        "{summary:?}"
    );
}

/// RED before SECRETS-OTHER: a proxy, MCP, node, OTEL, daemon or channel
/// endpoint URL carrying `user:password@` was a low-risk change whose
/// plan showed the password.
#[test]
fn a_plan_over_endpoint_url_userinfo_is_a_credential_change_and_renders_no_userinfo() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let after = fixture::config_from_overlay(dir.path(), fixture::ENDPOINT_USERINFO_TOML);
    let mut before = Config::default();
    before.config_path = after.config_path.clone();
    before.workspace_dir = after.workspace_dir.clone();
    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: plan");
    let rendered = serde_json::to_string(&summary).expect("test: json");
    assert!(
        !rendered.contains("userinfo-sentinel"),
        "a plan leaked userinfo: {rendered}"
    );
    assert_eq!(summary.risk, ConfigChangeRisk::High);
    for path in [
        "proxy.http_proxy",
        "proxy.all_proxy",
        "mcp.servers.remote.url",
        "chat.daemon.url",
        "nodes.nodes[0].endpoint",
        "observability.otel_endpoint",
        "channels_config.whatsapp.ws_url",
    ] {
        assert!(
            summary
                .risk_reasons
                .contains(&RiskReason::CredentialChanged { path: path.into() }),
            "{path}: {:?}",
            summary.risk_reasons
        );
    }
}

/// R-SECRETS (TOKEN_ENDPOINTS/URL_USERINFO_PATHS overlap), RED before
/// SECRETS-f: re-pointing a proxy, through which every credentialed
/// request travels, was a low-risk change. The unified endpoint registry
/// marks it a credential recipient.
#[test]
fn re_pointing_a_proxy_is_an_endpoint_change() {
    let mut before = Config::default();
    before.proxy.https_proxy = Some("http://proxy-a.example:3128".into());
    let mut after = before.clone();
    after.proxy.https_proxy = Some("http://proxy-b.example:3128".into());
    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
    assert!(
        summary.risk_reasons.contains(&RiskReason::EndpointChanged {
            path: "proxy.https_proxy".into()
        }),
        "{:?}",
        summary.risk_reasons
    );
}

/// R-SECRETS F1: an MCP launch environment value and a paired token are
/// not credentials alone; the plan gives each its own reason, so a
/// surface that confirms credential rotation implicitly (the credential
/// endpoint) cannot confirm them. A route key stays a credential.
#[test]
fn process_environment_and_access_grant_changes_have_their_own_reasons() {
    let mut before = Config::default();
    before.mcp.servers.insert(
        "local".into(),
        serde_json::from_value(serde_json::json!({"command": "mcp-local"})).expect("test: server"),
    );
    let mut after = before.clone();
    after
        .mcp
        .servers
        .get_mut("local")
        .expect("test: server")
        .env
        .insert("NODE_OPTIONS".into(), "--require /tmp/x.js".into());
    after.gateway.paired_tokens = vec!["zc_planted".into()];
    after.api_key = Some("sk-rotated".into());

    let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::High);
    assert_eq!(
        summary.risk_reasons,
        vec![
            RiskReason::CredentialChanged { path: "api_key".into() },
            RiskReason::ProcessEnvironmentChanged {
                path: "mcp.servers.local.env.NODE_OPTIONS".into()
            },
            RiskReason::AccessGrantChanged {
                path: "gateway.paired_tokens[0]".into()
            },
        ]
    );
    let rendered = serde_json::to_string(&summary).expect("test: json");
    assert!(
        !rendered.contains("/tmp/x.js") && !rendered.contains("zc_planted"),
        "{rendered}"
    );
}

/// D-048, RED before SECRETS-g: changing an admission secret other than a
/// paired token (the webhook token, a channel verify token, the node
/// server's bearer token, ...) was a plain credential change, so the
/// credential endpoint confirmed it implicitly. After: every registry
/// path marked as an access grant gives `AccessGrantChanged` for its own
/// path and nothing else; an outbound credential stays
/// `CredentialChanged`.
#[test]
fn every_admission_secret_change_is_an_access_grant_change() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let before = fixture::config_in(dir.path());
    let before_json = serde_json::to_value(&before).expect("test: render fixture");
    let fixture_tree: toml::Value = toml::from_str(fixture::TOML).expect("test: fixture parses");
    let sentinels = fixture::string_leaves(&fixture_tree, &|value| value.contains(fixture::SENTINEL_PREFIX));
    let pointer = |path: &[PathElem]| {
        path.iter()
            .map(|elem| match elem {
                PathElem::Key(key) => format!("/{key}"),
                PathElem::Index(index) => format!("/{index}"),
            })
            .collect::<String>()
    };
    let change = |path: &[PathElem]| {
        let mut changed = before_json.clone();
        let slot = changed
            .pointer_mut(&pointer(path))
            .unwrap_or_else(|| panic!("test: {} in the fixture", secret_paths::display_path(path)));
        *slot = serde_json::json!("rotated-admission-value");
        let mut after: Config = serde_json::from_value(changed).expect("test: changed config");
        after.config_path.clone_from(&before.config_path);
        after.workspace_dir.clone_from(&before.workspace_dir);
        ConfigChangeSummary::between(Some(&before), &after).expect("test: summary")
    };

    let mut grants = std::collections::BTreeSet::new();
    for path in &sentinels {
        let Some(registered) = secret_paths::registered(path) else {
            continue;
        };
        let display = secret_paths::display_path(path);
        let summary = change(path.as_slice());
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{display}");
        let expected = match registered.used_as {
            secret_paths::SecretUse::AccessGrant => {
                grants.insert(registered.pattern);
                RiskReason::AccessGrantChanged { path: display.clone() }
            }
            secret_paths::SecretUse::ProcessEnvironment => {
                RiskReason::ProcessEnvironmentChanged { path: display.clone() }
            }
            secret_paths::SecretUse::Credential => RiskReason::CredentialChanged { path: display.clone() },
        };
        let mut expected = vec![expected];
        // A connection string is also an endpoint (D-049 F).
        if secret_paths::is_credential_recipient_endpoint(path) {
            expected.push(RiskReason::EndpointChanged { path: display.clone() });
        }
        assert_eq!(summary.risk_reasons, expected, "{display}");
    }
    let registered: std::collections::BTreeSet<&str> = secret_paths::SECRET_PATHS
        .iter()
        .filter(|secret| secret.alias_of.is_none() && secret.used_as == secret_paths::SecretUse::AccessGrant)
        .map(|secret| secret.pattern)
        .collect();
    assert_eq!(grants, registered, "the fixture must reach every access grant");
}

/// D-049 A, RED before SECRETS-h: a change of a setting that decides who
/// is admitted inbound was Low. After: each is `AccessGrantChanged` for
/// its own path; a first write (no prior configuration) grants nothing
/// new; an unrelated setting stays Low.
#[test]
fn inbound_trust_settings_are_access_grants() {
    let before = Config::default();
    let before_json = serde_json::to_value(&before).expect("test: render");
    let changes: &[(&str, serde_json::Value, &str)] = &[
        (
            "/gateway/require_pairing",
            serde_json::json!(false),
            "gateway.require_pairing",
        ),
        (
            "/gateway/trust_forwarded_headers",
            serde_json::json!(true),
            "gateway.trust_forwarded_headers",
        ),
        (
            "/gateway/allow_public_bind",
            serde_json::json!(true),
            "gateway.allow_public_bind",
        ),
        ("/gateway/host", serde_json::json!("0.0.0.0"), "gateway.host"),
        (
            "/mcp_server/require_auth",
            serde_json::json!(false),
            "mcp_server.require_auth",
        ),
        (
            "/mcp_server/jwt_public_key_pem",
            serde_json::json!("-----BEGIN PUBLIC KEY-----"),
            "mcp_server.jwt_public_key_pem",
        ),
        (
            "/mcp_server/jwt_jwks",
            serde_json::json!("{\"keys\":[]}"),
            "mcp_server.jwt_jwks",
        ),
        (
            "/mcp_server/jwt_jwks_uri",
            serde_json::json!("https://jwks.invalid/keys"),
            "mcp_server.jwt_jwks_uri",
        ),
        (
            "/mcp_server/jwt_issuer",
            serde_json::json!("https://issuer.invalid"),
            "mcp_server.jwt_issuer",
        ),
        (
            "/mcp_server/jwt_audience",
            serde_json::json!("prx"),
            "mcp_server.jwt_audience",
        ),
        (
            "/mcp_server/jwt_algorithms",
            serde_json::json!(["HS256"]),
            "mcp_server.jwt_algorithms[0]",
        ),
        (
            "/a2a/allowed_peer_issuers",
            serde_json::json!(["spiffe://peer"]),
            "a2a.allowed_peer_issuers[0]",
        ),
        (
            "/a2a/trusted_trust_domains",
            serde_json::json!(["peer.invalid"]),
            "a2a.trusted_trust_domains[0]",
        ),
        (
            "/a2a/spiffe_trust_bundle_pem",
            serde_json::json!("-----BEGIN CERTIFICATE-----"),
            "a2a.spiffe_trust_bundle_pem",
        ),
        (
            "/a2a/spiffe_strict_validation",
            serde_json::json!(true),
            "a2a.spiffe_strict_validation",
        ),
        (
            "/identity_bindings",
            serde_json::json!([{"user_id": "owner", "channel": "telegram", "channel_account": "42"}]),
            "identity_bindings[0].",
        ),
    ];
    for (pointer, value, path) in changes {
        let mut changed = before_json.clone();
        *changed
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("test: {pointer} in the default configuration")) = value.clone();
        let after: Config = serde_json::from_value(changed).expect("test: changed config");
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{path}: {summary:?}");
        assert!(
            summary.risk_reasons.iter().any(|reason| matches!(
                reason,
                RiskReason::AccessGrantChanged { path: reported } if reported.starts_with(path)
            )),
            "{path}: {:?}",
            summary.risk_reasons
        );
    }

    let mut unrelated = before.clone();
    unrelated.gateway.port = 17000;
    let summary = ConfigChangeSummary::between(Some(&before), &unrelated).expect("test: summary");
    assert_eq!(summary.risk, ConfigChangeRisk::Low, "{summary:?}");

    let first = ConfigChangeSummary::between(None, &Config::default()).expect("test: first write");
    assert!(
        !first
            .risk_reasons
            .iter()
            .any(|reason| matches!(reason, RiskReason::AccessGrantChanged { .. })),
        "{:?}",
        first.risk_reasons
    );
}

fn section<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).expect("test: channel section")
}

fn policy(
    user: &str,
    role: &str,
    projects: &[&str],
    ceiling: &str,
    blocked: &[&str],
) -> crate::config::UserPolicyConfig {
    crate::config::UserPolicyConfig {
        user_id: user.to_string(),
        role: role.to_string(),
        projects: projects.iter().map(|p| (*p).to_string()).collect(),
        visibility_ceiling: ceiling.to_string(),
        blocked_patterns: blocked.iter().map(|p| (*p).to_string()).collect(),
    }
}

fn strings(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|entry| (*entry).to_string()).collect()
}

fn rule(channel: &str) -> crate::config::ScopeRule {
    crate::config::ScopeRule {
        channel: Some(channel.to_string()),
        ..crate::config::ScopeRule::default()
    }
}

/// One direction-aware case: the setup both sides share, the change, and
/// the path a widening reports (`None`: the change narrows, so it is low).
type Case = (&'static str, fn(&mut Config), fn(&mut Config), Option<&'static str>);

/// D-049 amendment, RED before SECRETS-h: each of these settings changed
/// as Low in both directions. After: the widening direction is high risk
/// with a reason naming the setting, and the narrowing direction stays
/// low, for every setting, explicitly.
#[test]
fn direction_aware_settings_are_high_only_when_they_widen() {
    fn discord(config: &mut Config, allowed: &[&str]) {
        config.channels_config.discord = Some(section(serde_json::json!({
            "bot_token": "t", "guild_id": "g1", "allowed_users": allowed
        })));
    }
    let cases: &[Case] = &[
        // ── Channel listening scope ──
        (
            "discord guild moved, everybody admitted",
            |c| discord(c, &["*"]),
            |c| c.channels_config.discord.as_mut().expect("t").guild_id = Some("g2".into()),
            Some("channels_config.discord.guild_id"),
        ),
        (
            "discord guild dropped (all guilds)",
            |c| discord(c, &["*"]),
            |c| c.channels_config.discord.as_mut().expect("t").guild_id = None,
            Some("channels_config.discord.guild_id"),
        ),
        (
            "discord guild moved, senders named",
            |c| discord(c, &["alice"]),
            |c| c.channels_config.discord.as_mut().expect("t").guild_id = Some("g2".into()),
            None,
        ),
        (
            "discord guild set where there was none (narrows)",
            |c| {
                discord(c, &["*"]);
                c.channels_config.discord.as_mut().expect("t").guild_id = None;
            },
            |c| c.channels_config.discord.as_mut().expect("t").guild_id = Some("g1".into()),
            None,
        ),
        (
            "slack channel moved, everybody admitted",
            |c| {
                c.channels_config.slack = Some(section(serde_json::json!({
                    "bot_token": "t", "channel_id": "C1", "allowed_users": ["*"]
                })));
            },
            |c| c.channels_config.slack.as_mut().expect("t").channel_id = Some("C2".into()),
            Some("channels_config.slack.channel_id"),
        ),
        (
            "mattermost channel moved, nobody named (empty list admits nobody)",
            |c| {
                c.channels_config.mattermost = Some(section(serde_json::json!({
                    "url": "http://127.0.0.1:1", "bot_token": "t", "channel_id": "c1", "allowed_users": []
                })));
            },
            |c| c.channels_config.mattermost.as_mut().expect("t").channel_id = Some("c2".into()),
            None,
        ),
        (
            "irc channel joined, everybody admitted",
            |c| {
                c.channels_config.irc = Some(section(serde_json::json!({
                    "server": "irc.invalid", "nickname": "prx", "channels": ["#a"], "allowed_users": ["*"]
                })));
            },
            |c| c.channels_config.irc.as_mut().expect("t").channels.push("#b".into()),
            Some("channels_config.irc.channels"),
        ),
        (
            "irc channel left (narrows)",
            |c| {
                c.channels_config.irc = Some(section(serde_json::json!({
                    "server": "irc.invalid", "nickname": "prx", "channels": ["#a", "#b"], "allowed_users": ["*"]
                })));
            },
            |c| {
                c.channels_config.irc.as_mut().expect("t").channels.pop();
            },
            None,
        ),
        (
            "signal group scope dropped, empty allow-list",
            |c| {
                c.channels_config.signal = Some(section(serde_json::json!({
                    "account": "+10000000000", "group_id": "grp"
                })));
            },
            |c| c.channels_config.signal.as_mut().expect("t").group_id = None,
            Some("channels_config.signal.group_id"),
        ),
        (
            "signal group scope dropped, senders named",
            |c| {
                c.channels_config.signal = Some(section(serde_json::json!({
                    "account": "+10000000000", "group_id": "grp", "allowed_from": ["+15550001111"]
                })));
            },
            |c| c.channels_config.signal.as_mut().expect("t").group_id = None,
            None,
        ),
        // ── user_policies ──
        (
            "a new policy grants owner",
            |_| {},
            |c| c.user_policies.push(policy("u1", "owner", &[], "private", &[])),
            Some("user_policies[0].role"),
        ),
        (
            "role lowered (narrows)",
            |c| c.user_policies.push(policy("u1", "owner", &[], "private", &[])),
            |c| c.user_policies.first_mut().expect("t").role = "guest".into(),
            None,
        ),
        (
            "project granted",
            |c| c.user_policies.push(policy("u1", "member", &["a"], "private", &[])),
            |c| c.user_policies.first_mut().expect("t").projects.push("b".into()),
            Some("user_policies[0].projects"),
        ),
        (
            "project withdrawn (narrows)",
            |c| {
                c.user_policies
                    .push(policy("u1", "member", &["a", "b"], "private", &[]))
            },
            |c| c.user_policies.first_mut().expect("t").projects.truncate(1),
            None,
        ),
        (
            "visibility ceiling raised",
            |c| c.user_policies.push(policy("u1", "member", &[], "private", &[])),
            |c| c.user_policies.first_mut().expect("t").visibility_ceiling = "public".into(),
            Some("user_policies[0].visibility_ceiling"),
        ),
        (
            "visibility ceiling lowered (narrows)",
            |c| c.user_policies.push(policy("u1", "member", &[], "public", &[])),
            |c| c.user_policies.first_mut().expect("t").visibility_ceiling = "private".into(),
            None,
        ),
        (
            "blocked pattern dropped",
            |c| c.user_policies.push(policy("u1", "member", &[], "private", &["token"])),
            |c| c.user_policies.first_mut().expect("t").blocked_patterns.clear(),
            Some("user_policies[0].blocked_patterns"),
        ),
        (
            "blocked pattern added (narrows)",
            |c| c.user_policies.push(policy("u1", "member", &[], "private", &[])),
            |c| {
                c.user_policies
                    .first_mut()
                    .expect("t")
                    .blocked_patterns
                    .push("token".into())
            },
            None,
        ),
        (
            "a restricting policy removed (its blocked patterns go)",
            |c| c.user_policies.push(policy("u1", "guest", &[], "private", &["token"])),
            |c| c.user_policies.clear(),
            Some("user_policies[0].blocked_patterns"),
        ),
        (
            "a member policy removed (back to guest, narrows)",
            |c| c.user_policies.push(policy("u1", "member", &[], "private", &[])),
            |c| c.user_policies.clear(),
            None,
        ),
        // ── autonomy.scopes ──
        (
            "default deny to allow",
            |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Deny,
            |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Allow,
            Some("autonomy.scopes.default"),
        ),
        (
            "default allow to deny (narrows)",
            |_| {},
            |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Deny,
            None,
        ),
        (
            "assignment owner added",
            |_| {},
            |c| c.autonomy.scopes.assign_owners.push("telegram:alice".into()),
            Some("autonomy.scopes.assign_owners"),
        ),
        (
            "assignment owner removed (narrows)",
            |c| c.autonomy.scopes.assign_owners.push("telegram:alice".into()),
            |c| c.autonomy.scopes.assign_owners.clear(),
            None,
        ),
        (
            "tools_deny entry dropped",
            |c| {
                let mut r = rule("signal");
                r.tools_deny = strings(&["shell"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| c.autonomy.scopes.rules.first_mut().expect("t").tools_deny.clear(),
            Some("autonomy.scopes.rules[0].tools_deny"),
        ),
        (
            "tools_deny entry added (narrows)",
            |c| c.autonomy.scopes.rules.push(rule("signal")),
            |c| {
                c.autonomy
                    .scopes
                    .rules
                    .first_mut()
                    .expect("t")
                    .tools_deny
                    .push("shell".into())
            },
            None,
        ),
        (
            "tools_allow whitelist gains a tool",
            |c| {
                let mut r = rule("signal");
                r.tools_allow = strings(&["memory_recall"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| {
                c.autonomy
                    .scopes
                    .rules
                    .first_mut()
                    .expect("t")
                    .tools_allow
                    .push("shell".into())
            },
            Some("autonomy.scopes.rules[0].tools_allow"),
        ),
        (
            "tools_allow whitelist emptied (every tool)",
            |c| {
                let mut r = rule("signal");
                r.tools_allow = strings(&["memory_recall"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| c.autonomy.scopes.rules.first_mut().expect("t").tools_allow.clear(),
            Some("autonomy.scopes.rules[0].tools_allow"),
        ),
        (
            "tools_allow whitelist introduced (narrows)",
            |c| c.autonomy.scopes.rules.push(rule("signal")),
            |c| {
                c.autonomy
                    .scopes
                    .rules
                    .first_mut()
                    .expect("t")
                    .tools_allow
                    .push("memory_recall".into())
            },
            None,
        ),
        (
            "send_deny entry dropped",
            |c| {
                let mut r = rule("wacli");
                r.send_deny = strings(&["*:+15550001111"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| c.autonomy.scopes.rules.first_mut().expect("t").send_deny.clear(),
            Some("autonomy.scopes.rules[0].send_deny"),
        ),
        (
            "send_allow whitelist gains a recipient",
            |c| {
                let mut r = rule("wacli");
                r.send_allow = strings(&["wacli:1"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| {
                c.autonomy
                    .scopes
                    .rules
                    .first_mut()
                    .expect("t")
                    .send_allow
                    .push("wacli:2".into())
            },
            Some("autonomy.scopes.rules[0].send_allow"),
        ),
        (
            "assign_allow entry added (assignment is default-deny)",
            |c| c.autonomy.scopes.rules.push(rule("telegram")),
            |c| {
                c.autonomy
                    .scopes
                    .rules
                    .first_mut()
                    .expect("t")
                    .assign_allow
                    .push("*:*".into())
            },
            Some("autonomy.scopes.rules[0].assign_allow"),
        ),
        (
            "assign_allow emptied with no later rule (falls through to default-deny, narrows)",
            |c| {
                let mut r = rule("telegram");
                r.assign_allow = strings(&["*:*"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| c.autonomy.scopes.rules.first_mut().expect("t").assign_allow.clear(),
            None,
        ),
        (
            "assign_deny entry dropped",
            |c| {
                let mut r = rule("telegram");
                r.assign_deny = strings(&["*:*"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| c.autonomy.scopes.rules.first_mut().expect("t").assign_deny.clear(),
            Some("autonomy.scopes.rules[0].assign_deny"),
        ),
        (
            "deny-only rule appended under default allow (narrows)",
            |_| {},
            |c| {
                let mut r = rule("signal");
                r.tools_deny = strings(&["shell"]);
                c.autonomy.scopes.rules.push(r);
            },
            None,
        ),
        (
            "rule appended under default deny (admits what default refused)",
            |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Deny,
            |c| {
                let mut r = rule("signal");
                r.tools_deny = strings(&["shell"]);
                c.autonomy.scopes.rules.push(r);
            },
            Some("autonomy.scopes.rules[0]"),
        ),
        (
            "rule with a whitelist appended",
            |_| {},
            |c| {
                let mut r = rule("signal");
                r.send_allow = strings(&["signal:*"]);
                c.autonomy.scopes.rules.push(r);
            },
            Some("autonomy.scopes.rules[0]"),
        ),
        (
            "rule removed",
            |c| {
                let mut r = rule("signal");
                r.tools_deny = strings(&["shell"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| c.autonomy.scopes.rules.clear(),
            Some("autonomy.scopes.rules"),
        ),
        (
            "rule matcher changed",
            |c| c.autonomy.scopes.rules.push(rule("signal")),
            |c| c.autonomy.scopes.rules.first_mut().expect("t").channel = Some("telegram".into()),
            Some("autonomy.scopes.rules"),
        ),
        // ── MCP endpoint and node server ──
        (
            "a tool newly exposed on the MCP endpoint",
            |_| {},
            |c| c.mcp_server.exposed_tools.push("shell".into()),
            Some("mcp_server.exposed_tools"),
        ),
        (
            "a tool no longer exposed (narrows)",
            |_| {},
            |c| {
                c.mcp_server.exposed_tools.pop();
            },
            None,
        ),
        (
            "node server TLS requirement turned off",
            |c| c.nodes.server.tls_required = true,
            |c| c.nodes.server.tls_required = false,
            Some("nodes.server.tls_required"),
        ),
        (
            "node server TLS requirement turned on (narrows)",
            |c| c.nodes.server.tls_required = false,
            |c| c.nodes.server.tls_required = true,
            None,
        ),
        // ── D-050 item 2: list fields by their runtime meaning ──
        (
            "send_allow introduced on a tools-only rule (R-SECRETSfg P21)",
            |c| {
                let mut r = rule("signal");
                r.user = Some("alice".into());
                r.tools_deny = strings(&["x"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| c.autonomy.scopes.rules.first_mut().expect("t").send_allow = strings(&["*"]),
            Some("autonomy.scopes.rules[0].send_allow"),
        ),
        (
            "send_allow entry removed, one left (narrows)",
            |c| {
                let mut r = rule("wacli");
                r.send_allow = strings(&["wacli:1", "wacli:2"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| {
                c.autonomy.scopes.rules.first_mut().expect("t").send_allow.pop();
            },
            None,
        ),
        (
            "send_allow emptied (falls through to a later whitelist or the reply channel)",
            |c| {
                let mut r = rule("wacli");
                r.send_allow = strings(&["wacli:1"]);
                c.autonomy.scopes.rules.push(r);
            },
            |c| c.autonomy.scopes.rules.first_mut().expect("t").send_allow.clear(),
            Some("autonomy.scopes.rules[0].send_allow"),
        ),
        (
            "assign_allow emptied while a later overlapping rule has one (R-SECRETSfg P22)",
            |c| {
                let mut alice = crate::config::ScopeRule {
                    user: Some("alice".into()),
                    ..crate::config::ScopeRule::default()
                };
                alice.assign_allow = strings(&["lab:s1"]);
                let mut telegram = rule("telegram");
                telegram.assign_allow = strings(&["*:*"]);
                c.autonomy.scopes.rules.push(alice);
                c.autonomy.scopes.rules.push(telegram);
            },
            |c| c.autonomy.scopes.rules.first_mut().expect("t").assign_allow.clear(),
            Some("autonomy.scopes.rules[0].assign_allow"),
        ),
        (
            "assign_allow emptied while the later rule cannot match the same requests (narrows)",
            |c| {
                let mut alice = crate::config::ScopeRule {
                    user: Some("alice".into()),
                    ..crate::config::ScopeRule::default()
                };
                alice.assign_allow = strings(&["lab:s1"]);
                let mut bob = crate::config::ScopeRule {
                    user: Some("bob".into()),
                    ..crate::config::ScopeRule::default()
                };
                bob.assign_allow = strings(&["*:*"]);
                c.autonomy.scopes.rules.push(alice);
                c.autonomy.scopes.rules.push(bob);
            },
            |c| c.autonomy.scopes.rules.first_mut().expect("t").assign_allow.clear(),
            None,
        ),
        // ── D-050 item 4: exposure and execution ──
        (
            "a tunnel switched on",
            |_| {},
            |c| c.tunnel.provider = "tailscale".into(),
            Some("tunnel.provider"),
        ),
        (
            "the tunnel switched off (narrows)",
            |c| c.tunnel.provider = "tailscale".into(),
            |c| c.tunnel.provider = "none".into(),
            None,
        ),
        (
            "Tailscale funnel turned on (public)",
            |c| {
                c.tunnel.provider = "tailscale".into();
                c.tunnel.tailscale = Some(section(serde_json::json!({"funnel": false})));
            },
            |c| c.tunnel.tailscale = Some(section(serde_json::json!({"funnel": true}))),
            Some("tunnel.tailscale.funnel"),
        ),
        (
            "Tailscale funnel turned off (narrows)",
            |c| {
                c.tunnel.provider = "tailscale".into();
                c.tunnel.tailscale = Some(section(serde_json::json!({"funnel": true})));
            },
            |c| c.tunnel.tailscale = Some(section(serde_json::json!({"funnel": false}))),
            None,
        ),
        (
            "custom tunnel command set",
            |_| {},
            |c| c.tunnel.custom = Some(section(serde_json::json!({"start_command": "tunnel-up"}))),
            Some("tunnel.custom.start_command"),
        ),
        (
            "custom tunnel command changed",
            |c| c.tunnel.custom = Some(section(serde_json::json!({"start_command": "tunnel-up"}))),
            |c| c.tunnel.custom = Some(section(serde_json::json!({"start_command": "/tmp/other"}))),
            Some("tunnel.custom.start_command"),
        ),
        (
            "custom tunnel removed (narrows)",
            |c| c.tunnel.custom = Some(section(serde_json::json!({"start_command": "tunnel-up"}))),
            |c| c.tunnel.custom = None,
            None,
        ),
        (
            "autonomy raised from supervised to full",
            |c| c.autonomy.level = crate::security::AutonomyLevel::Supervised,
            |c| c.autonomy.level = crate::security::AutonomyLevel::Full,
            Some("autonomy.level"),
        ),
        (
            "autonomy lowered to read-only (narrows)",
            |_| {},
            |c| c.autonomy.level = crate::security::AutonomyLevel::ReadOnly,
            None,
        ),
        (
            "workspace_only turned off",
            |c| c.autonomy.workspace_only = true,
            |c| c.autonomy.workspace_only = false,
            Some("autonomy.workspace_only"),
        ),
        (
            "workspace_only turned on (narrows)",
            |c| c.autonomy.workspace_only = false,
            |c| c.autonomy.workspace_only = true,
            None,
        ),
        (
            "a forbidden path removed",
            |c| c.autonomy.forbidden_paths = vec!["/etc".into()],
            |c| {
                c.autonomy.forbidden_paths.pop();
            },
            Some("autonomy.forbidden_paths"),
        ),
        (
            "a forbidden path added (narrows)",
            |_| {},
            |c| c.autonomy.forbidden_paths.push("/srv".into()),
            None,
        ),
        (
            "a node command allowed",
            |c| c.nodes.server.allowed_commands = strings(&["echo"]),
            |c| c.nodes.server.allowed_commands.push("sh".into()),
            Some("nodes.server.allowed_commands"),
        ),
        (
            "a node command no longer allowed (narrows)",
            |c| c.nodes.server.allowed_commands = strings(&["echo", "ls"]),
            |c| c.nodes.server.allowed_commands = strings(&["echo"]),
            None,
        ),
        (
            "a node command unblocked",
            |c| c.nodes.server.blocked_commands = strings(&["rm"]),
            |c| c.nodes.server.blocked_commands.clear(),
            Some("nodes.server.blocked_commands"),
        ),
        (
            "a node command blocked (narrows)",
            |_| {},
            |c| c.nodes.server.blocked_commands.push("rm".into()),
            None,
        ),
        // ── D-051: containment ──
        (
            "working directory set",
            |_| {},
            |c| c.runtime.workdir = Some(std::env::temp_dir()),
            Some("runtime.workdir"),
        ),
        (
            "working directory changed",
            |c| c.runtime.workdir = Some(std::env::temp_dir()),
            |c| c.runtime.workdir = Some(std::env::temp_dir().join("prx-other-root")),
            Some("runtime.workdir"),
        ),
        (
            "working directory unset",
            |c| c.runtime.workdir = Some(std::env::temp_dir()),
            |c| c.runtime.workdir = None,
            Some("runtime.workdir"),
        ),
        (
            "shell gains a host environment variable",
            |_| {},
            |c| c.runtime.shell_env_allowlist = strings(&["PRX_TEST_TOKEN"]),
            Some("runtime.shell_env_allowlist"),
        ),
        (
            "shell loses a host environment variable (narrows)",
            |c| c.runtime.shell_env_allowlist = strings(&["PRX_TEST_TOKEN"]),
            |c| c.runtime.shell_env_allowlist.clear(),
            None,
        ),
        (
            "sandbox removed: runtime.kind docker to native (R-SECRETSfg P27)",
            |c| c.runtime.kind = "docker".into(),
            |c| c.runtime.kind = "native".into(),
            Some("runtime.kind"),
        ),
        (
            "sandbox added: runtime.kind native to docker (narrows)",
            |c| c.runtime.kind = "native".into(),
            |c| c.runtime.kind = "docker".into(),
            None,
        ),
        (
            "docker network opened to the host (R-SECRETSfg P27)",
            |c| c.runtime.docker.network = "none".into(),
            |c| c.runtime.docker.network = "host".into(),
            Some("runtime.docker.network"),
        ),
        (
            "docker network from none to the default bridge",
            |c| c.runtime.docker.network = "none".into(),
            |c| c.runtime.docker.network = String::new(),
            Some("runtime.docker.network"),
        ),
        (
            "docker network moved to another named network",
            |c| c.runtime.docker.network = "sandbox-a".into(),
            |c| c.runtime.docker.network = "sandbox-b".into(),
            Some("runtime.docker.network"),
        ),
        (
            "docker network from host to none (narrows)",
            |c| c.runtime.docker.network = "host".into(),
            |c| c.runtime.docker.network = "none".into(),
            None,
        ),
        (
            "docker network from host to a named network (narrows)",
            |c| c.runtime.docker.network = "host".into(),
            |c| c.runtime.docker.network = "sandbox-a".into(),
            None,
        ),
        (
            "operator tools exposed to channel sessions (R-SECRETSfg P27)",
            |c| c.tool_tiering = crate::config::ToolTieringConfig::with_recommended_channel_exclusions(),
            |c| c.tool_tiering.channel_exclude.clear(),
            Some("tool_tiering"),
        ),
        (
            "an operator tool hidden from channel sessions (narrows)",
            |_| {},
            |c| c.tool_tiering.channel_exclude.push("shell".into()),
            None,
        ),
        (
            "docker root filesystem made writable",
            |c| c.runtime.docker.read_only_rootfs = true,
            |c| c.runtime.docker.read_only_rootfs = false,
            Some("runtime.docker.read_only_rootfs"),
        ),
        (
            "docker root filesystem made read-only (narrows)",
            |c| c.runtime.docker.read_only_rootfs = false,
            |c| c.runtime.docker.read_only_rootfs = true,
            None,
        ),
        (
            "a docker workspace root added",
            |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a"]),
            |c| c.runtime.docker.allowed_workspace_roots.push("/".into()),
            Some("runtime.docker.allowed_workspace_roots"),
        ),
        (
            "docker workspace roots emptied (any workspace)",
            |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a"]),
            |c| c.runtime.docker.allowed_workspace_roots.clear(),
            Some("runtime.docker.allowed_workspace_roots"),
        ),
        (
            "a docker workspace root removed, one left (narrows)",
            |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a", "/srv/b"]),
            |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a"]),
            None,
        ),
        (
            "docker workspace roots introduced (narrows)",
            |_| {},
            |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a"]),
            None,
        ),
        (
            "a fetchable domain added",
            |c| c.browser.allowed_domains = strings(&["example.com"]),
            |c| c.browser.allowed_domains.push("attacker.invalid".into()),
            Some("browser.allowed_domains"),
        ),
        (
            "a wildcard domain added",
            |c| c.browser.allowed_domains = strings(&["example.com"]),
            |c| c.browser.allowed_domains.push("*".into()),
            Some("browser.allowed_domains"),
        ),
        (
            "fetchable domains emptied (any public host)",
            |c| c.browser.allowed_domains = strings(&["example.com"]),
            |c| c.browser.allowed_domains.clear(),
            Some("browser.allowed_domains"),
        ),
        (
            "a fetchable domain respelled the way the tool normalises it (no change)",
            |c| c.browser.allowed_domains = strings(&["example.com"]),
            |c| c.browser.allowed_domains = strings(&["https://Example.com/"]),
            None,
        ),
        (
            "a fetchable domain removed, one left (narrows)",
            |c| c.browser.allowed_domains = strings(&["example.com", "example.org"]),
            |c| c.browser.allowed_domains = strings(&["example.com"]),
            None,
        ),
        (
            "fetchable domains introduced (narrows)",
            |_| {},
            |c| c.browser.allowed_domains = strings(&["example.com"]),
            None,
        ),
        (
            "an excluded tool re-admitted through always_include (R-SECRETSj P27b)",
            |_| {},
            |c| c.tool_tiering.always_include.push("proxy_config".into()),
            Some("tool_tiering"),
        ),
        (
            "an always_include entry removed (narrows)",
            |c| c.tool_tiering.always_include.push("proxy_config".into()),
            |c| c.tool_tiering.always_include.clear(),
            None,
        ),
        (
            "an always_exclude entry removed (offered on every surface)",
            |c| c.tool_tiering.always_exclude.push("shell".into()),
            |c| c.tool_tiering.always_exclude.clear(),
            Some("tool_tiering"),
        ),
        (
            "always_exclude cleared while always_include names the same tool (R-SECRETSj K-1)",
            |c| {
                c.tool_tiering.always_exclude.push("shell".into());
                c.tool_tiering.always_include.push("shell".into());
            },
            |c| c.tool_tiering.always_exclude.clear(),
            Some("tool_tiering"),
        ),
        (
            "always_include naming an always_exclude tool changes nothing (K-1)",
            |c| c.tool_tiering.always_exclude.push("shell".into()),
            |c| c.tool_tiering.always_include.push("shell".into()),
            None,
        ),
        (
            "an always_exclude entry added (narrows)",
            |_| {},
            |c| c.tool_tiering.always_exclude.push("shell".into()),
            None,
        ),
        (
            "the host workspace mounted into the container",
            |c| c.runtime.docker.mount_workspace = false,
            |c| c.runtime.docker.mount_workspace = true,
            Some("runtime.docker.mount_workspace"),
        ),
        (
            "the host workspace no longer mounted (narrows)",
            |c| c.runtime.docker.mount_workspace = true,
            |c| c.runtime.docker.mount_workspace = false,
            None,
        ),
        (
            "the audit trail shortened",
            |c| c.security.audit.max_size_mb = 100,
            |c| c.security.audit.max_size_mb = 1,
            Some("security.audit.max_size_mb"),
        ),
        (
            "the audit trail lengthened (narrows)",
            |c| c.security.audit.max_size_mb = 10,
            |c| c.security.audit.max_size_mb = 100,
            None,
        ),
        (
            "the audit log moved",
            |_| {},
            |c| c.security.audit.log_path = "logs/other.log".into(),
            Some("security.audit.log_path"),
        ),
        // ── D-050 item 1: one canonical reading ──
        (
            "scope default from deny to allow",
            |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Deny,
            |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Allow,
            Some("autonomy.scopes.default"),
        ),
    ];
    for (name, setup, change, widened) in cases {
        let mut before = Config::default();
        setup(&mut before);
        let mut after = before.clone();
        change(&mut after);
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        match widened {
            Some(path) => {
                assert_eq!(summary.risk, ConfigChangeRisk::High, "{name}: {summary:?}");
                assert!(
                    summary.risk_reasons.iter().any(|reason| matches!(
                        reason,
                        RiskReason::AccessGrantChanged { path: reported }
                            | RiskReason::PrivilegeWidened { path: reported } if reported == path
                    )),
                    "{name}: {:?}",
                    summary.risk_reasons
                );
            }
            None => assert_eq!(summary.risk, ConfigChangeRisk::Low, "{name}: {summary:?}"),
        }
    }
}
