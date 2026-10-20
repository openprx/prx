//! Credential containment across provider identities: the root connection and
//! its rotation keys (`reliability.api_keys`) reach only the provider they were
//! configured for, whatever path builds the chain.
#![cfg(test)]

use super::credential_probe::{CredentialProbe, PROBE_MODEL};
use super::*;

/// Sentinels only; none of these is a real secret.
const ROOT_SENTINEL: &str = "C0-ROOT-SENTINEL";
const ROTATION_SENTINEL: &str = "C0-ROTATION-SENTINEL";

fn isolated_options(temp: &tempfile::TempDir) -> ProviderRuntimeOptions {
    ProviderRuntimeOptions {
        openprx_dir: Some(temp.path().to_path_buf()),
        codex_auth_json_path: Some(temp.path().join("no-codex-auth.json")),
        codex_auth_json_auto_import: false,
        ..ProviderRuntimeOptions::default()
    }
}

fn rotating_reliability() -> crate::config::ReliabilityConfig {
    crate::config::ReliabilityConfig {
        provider_retries: 0,
        provider_backoff_ms: 0,
        fallback_providers: Vec::new(),
        api_keys: vec![ROTATION_SENTINEL.to_string()],
        ..crate::config::ReliabilityConfig::default()
    }
}

fn routed_config(
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    routes: Vec<crate::config::ModelRouteConfig>,
) -> crate::config::Config {
    crate::config::Config {
        default_provider: Some(provider.to_string()),
        default_model: Some(model.to_string()),
        api_key: api_key.map(str::to_string),
        api_url: api_url.map(str::to_string),
        reliability: rotating_reliability(),
        model_routes: routes,
        ..crate::config::Config::default()
    }
}

/// T7: a `model_routes` entry for another provider must not inherit the
/// root's rotation keys. The loopback answers 429 first, which is exactly
/// what makes the chain rotate to its next member.
#[tokio::test]
async fn a_routed_provider_never_receives_the_root_rotation_keys() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "probe".to_string(),
        provider: probe.custom_provider(),
        model: PROBE_MODEL.to_string(),
        api_key: None,
        credential_ref: None,
    }];

    let config = routed_config("openai", "gpt-4o", Some(ROOT_SENTINEL), None, routes);
    let provider =
        create_provider_from_test_config(&config, &isolated_options(&temp)).expect("test: routed chain builds");

    let _reply = provider.simple_chat("ping", "hint:probe", 0.0).await;

    let requests = probe.requests();
    assert!(!requests.is_empty(), "the route must reach the loopback");
    assert!(
        !probe.saw(ROTATION_SENTINEL),
        "the root rotation key reached a routed provider it was not issued for"
    );
    assert!(!probe.saw(ROOT_SENTINEL), "the root key reached a routed provider");
    assert!(
        requests.iter().all(|request| request.authorization().is_none()),
        "a keyless routed endpoint must receive no Authorization header"
    );
}

/// Guard: the primary provider keeps its rotation keys when routing is on.
/// Its first member is throttled, so the rotated member presents the key.
#[tokio::test]
async fn the_routed_primary_keeps_its_rotation_keys() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "probe".to_string(),
        provider: probe.custom_provider(),
        model: PROBE_MODEL.to_string(),
        api_key: None,
        credential_ref: None,
    }];

    let config = routed_config(&probe.custom_provider(), PROBE_MODEL, Some(ROOT_SENTINEL), None, routes);
    let provider =
        create_provider_from_test_config(&config, &isolated_options(&temp)).expect("test: routed chain builds");

    let reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

    assert!(reply.is_ok(), "the rotated member must answer: {reply:?}");
    let requests = probe.requests();
    assert_eq!(requests.len(), 2, "one throttled attempt, one rotated attempt");
    assert!(
        requests
            .iter()
            .all(|request| request.method == "POST" && request.path.ends_with("/chat/completions"))
    );
    assert_eq!(
        requests.first().and_then(|request| request.authorization()),
        Some(format!("Bearer {ROOT_SENTINEL}").as_str())
    );
    assert_eq!(
        requests.get(1).and_then(|request| request.authorization()),
        Some(format!("Bearer {ROTATION_SENTINEL}").as_str())
    );
}

/// A route's own key is that route's credential and is still presented.
#[tokio::test]
async fn a_route_with_its_own_key_still_presents_it() {
    const ROUTE_SENTINEL: &str = "C0-ROUTE-SENTINEL";
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start().await;
    let routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "probe".to_string(),
        provider: probe.custom_provider(),
        model: PROBE_MODEL.to_string(),
        api_key: Some(ROUTE_SENTINEL.to_string()),
        credential_ref: None,
    }];

    let config = routed_config("openai", "gpt-4o", Some(ROOT_SENTINEL), None, routes);
    let provider =
        create_provider_from_test_config(&config, &isolated_options(&temp)).expect("test: routed chain builds");

    let reply = provider.simple_chat("ping", "hint:probe", 0.0).await;

    assert!(reply.is_ok(), "the route answers with its own key: {reply:?}");
    assert_eq!(probe.wait_for_requests(1, std::time::Duration::from_secs(5)).await, 1);
    assert!(probe.saw(ROUTE_SENTINEL));
    assert!(!probe.saw(ROOT_SENTINEL) && !probe.saw(ROTATION_SENTINEL));
}

#[test]
fn rotation_keys_follow_the_root_connection() {
    let reliability = rotating_reliability();

    let released = configured_connection_for("OpenAI", "openai", Some(ROOT_SENTINEL), None);
    let scoped = scoped_reliability(&reliability, &released);
    assert!(released.released);
    assert!(matches!(scoped, std::borrow::Cow::Borrowed(_)));
    assert_eq!(scoped.api_keys, vec![ROTATION_SENTINEL.to_string()]);

    let withheld = configured_connection_for("custom:http://127.0.0.1:9/v1", "openai", Some(ROOT_SENTINEL), None);
    let scoped = scoped_reliability(&reliability, &withheld);
    assert!(!withheld.released);
    assert!(
        scoped.api_keys.is_empty(),
        "a withheld connection carries no rotation keys"
    );
    assert_eq!(scoped.provider_retries, reliability.provider_retries);
    assert_eq!(scoped.provider_backoff_ms, reliability.provider_backoff_ms);
    assert_eq!(scoped.fallback_providers, reliability.fallback_providers);
}

/// The factory-level shape of P0-B: a withheld connection plus the scoped
/// reliability sends neither the root key nor a rotation key.
#[tokio::test]
async fn a_withheld_connection_builds_a_chain_without_rotation_keys() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let selected = probe.custom_provider();
    let config = routed_config("openai", "gpt-4o", Some(ROOT_SENTINEL), None, Vec::new());
    let catalog = selection::catalog_for(&config);
    let resolved = selection::check_with(
        &config,
        &catalog,
        &selected,
        PROBE_MODEL,
        selection::credential_for(&config, &selected),
        &catalog::Requirement::chat(),
        catalog::UnknownPolicy::Invocation,
    )
    .expect("test: keyless BYO selection resolves");
    let provider = create_provider_for(
        &resolved,
        &catalog,
        &config.reliability,
        &config.cost,
        &isolated_options(&temp),
    )
    .expect("test: keyless BYO chain builds");
    let _reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

    assert_eq!(probe.requests().len(), 1, "one member, no rotated member");
    assert!(!probe.saw(ROOT_SENTINEL) && !probe.saw(ROTATION_SENTINEL));
}

#[test]
fn configured_providers_are_every_provider_the_operator_named() {
    let mut config = crate::config::Config::default();
    config.default_provider = None;
    config.reliability.fallback_providers = vec!["anthropic".to_string()];
    config.model_routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "custom:http://127.0.0.1:8080/v1".to_string(),
        model: "m".to_string(),
        api_key: None,
        credential_ref: None,
    }];
    config.agents = std::collections::HashMap::from([(
        "coder".to_string(),
        crate::config::DelegateAgentConfig {
            provider: "groq".to_string(),
            model: "m".to_string(),
            system_prompt: None,
            api_key: None,
            credential_ref: None,
            temperature: None,
            max_depth: 3,
            agentic: false,
            allowed_tools: Vec::new(),
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: None,
        },
    )]);

    let configured = ConfiguredProviders::from_config(&config);

    for permitted in [
        "openrouter",
        "anthropic",
        "claude-code",
        "groq",
        "custom:http://127.0.0.1:8080/v1/",
        "CUSTOM:http://127.0.0.1:8080/v1",
    ] {
        assert!(configured.permits(permitted), "{permitted}");
    }
    for refused in [
        "openai",
        "custom:http://127.0.0.1:8081/v1",
        "custom:https://attacker.example/v1",
    ] {
        assert!(!configured.permits(refused), "{refused}");
    }
}

#[test]
fn a_refused_provider_is_named_without_its_userinfo() {
    let configured =
        ConfiguredProviders::default().with_providers(["custom:http://operator-secret@127.0.0.1:8080/v1", "openai"]);

    let error = configured
        .ensure_permits("custom:https://model-secret@attacker.example/v1")
        .expect_err("an unconfigured endpoint is refused")
        .to_string();

    assert!(error.contains("not configured"), "{error}");
    assert!(
        !error.contains("model-secret") && !error.contains("operator-secret"),
        "{error}"
    );
    assert!(configured.ensure_permits("OpenAI").is_ok());
}

/// C0-f F-3: the refusal goes back to the model, so it names provider
/// identities only: never a configured endpoint's URL, host or port, and
/// never the refused one's.
#[test]
fn a_refusal_names_identities_but_no_endpoint() {
    let configured = ConfiguredProviders::default().with_providers([
        "custom:http://internal-llm.corp.example:8443/v1",
        "anthropic-custom:https://10.1.2.3/anthropic",
        "openai",
    ]);

    let error = configured
        .ensure_permits("custom:https://attacker.example:9999/v1")
        .expect_err("an unconfigured endpoint is refused")
        .to_string();

    for leaked in [
        "internal-llm",
        "corp.example",
        "8443",
        "10.1.2.3",
        "attacker.example",
        "9999",
        "http",
    ] {
        assert!(!error.contains(leaked), "the refusal leaked `{leaked}`: {error}");
    }
    assert!(error.contains("not configured"), "{error}");
    assert!(error.contains("openai"), "catalog identities stay nameable: {error}");
}

#[test]
fn a_credential_is_released_only_to_its_own_provider_identity() {
    let owned = [
        (Some("groq"), Some(" agent-key ")),
        (Some("custom:http://127.0.0.1:8080/v1"), Some("root-key")),
        (None, Some("ownerless-key")),
    ];
    assert_eq!(credential_for_provider_identity("GROQ", &owned), Some("agent-key"));
    assert_eq!(
        credential_for_provider_identity("custom:http://127.0.0.1:8080/v1/", &owned),
        Some("root-key")
    );
    assert_eq!(
        credential_for_provider_identity("custom:https://attacker.example/v1", &owned),
        None
    );
    assert_eq!(credential_for_provider_identity("openai", &owned), None);
    assert_eq!(
        credential_for_provider_identity("groq", &[(Some("groq"), Some("  "))]),
        None
    );
}

// ── C0-f F-4: a route gets key, URL and rotation keys as one unit ─────

#[test]
fn a_respelled_primary_route_gets_the_root_connection_as_a_unit() {
    let config = routed_config(
        "openai",
        "gpt-4o",
        Some(ROOT_SENTINEL),
        Some("https://gateway.corp.example/v1"),
        vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "primary".to_string(),
            provider: "OpenAI".to_string(),
            model: "gpt-4o".to_string(),
            api_key: None,
            credential_ref: None,
        }],
    );
    let catalog = selection::catalog_for(&config);
    let route = config.model_routes.first().expect("test: primary route");
    let resolved = selection::resolve_route(&config, &catalog, route, catalog::UnknownPolicy::Invocation)
        .expect("test: respelled primary resolves");
    assert_eq!(resolved.credential().api_key(), Some(ROOT_SENTINEL));
    assert_eq!(resolved.credential().api_url(), Some("https://gateway.corp.example/v1"));
    assert_eq!(resolved.credential().rotation_keys(), &[ROTATION_SENTINEL.to_string()]);
}

#[test]
fn a_route_with_its_own_key_gets_neither_root_url_nor_rotation_keys() {
    let config = routed_config(
        "openai",
        "gpt-4o",
        Some(ROOT_SENTINEL),
        Some("https://gateway.corp.example/v1"),
        vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "owned".to_string(),
            provider: "OpenAI".to_string(),
            model: "gpt-4o".to_string(),
            api_key: Some("route-own-key".to_string()),
            credential_ref: None,
        }],
    );
    let catalog = selection::catalog_for(&config);
    let route = config.model_routes.first().expect("test: owned route");
    let resolved = selection::resolve_route(&config, &catalog, route, catalog::UnknownPolicy::Invocation)
        .expect("test: owned route resolves");
    assert_eq!(resolved.credential().api_key(), Some("route-own-key"));
    assert_eq!(resolved.credential().api_url(), None);
    assert!(
        resolved.credential().rotation_keys().is_empty(),
        "the root's rotation keys are not the route's"
    );
}

#[test]
fn the_exact_primary_and_foreign_routes_are_unchanged() {
    let config = routed_config(
        "openai",
        "gpt-4o",
        Some(ROOT_SENTINEL),
        Some("https://gw/v1"),
        vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "foreign".to_string(),
            provider: "groq".to_string(),
            model: "llama-3.3-70b-versatile".to_string(),
            api_key: None,
            credential_ref: None,
        }],
    );
    let catalog = selection::catalog_for(&config);
    let primary =
        selection::check_default(&config, catalog::UnknownPolicy::Invocation).expect("test: primary resolves");
    assert_eq!(
        (primary.credential().api_key(), primary.credential().api_url()),
        (Some(ROOT_SENTINEL), Some("https://gw/v1"))
    );
    assert_eq!(primary.credential().rotation_keys().len(), 1);

    let route = config.model_routes.first().expect("test: foreign route");
    let foreign = selection::resolve_route(&config, &catalog, route, catalog::UnknownPolicy::Invocation)
        .expect("test: foreign route resolves");
    assert_eq!(
        (foreign.credential().api_key(), foreign.credential().api_url()),
        (None, None)
    );
    assert!(
        foreign.credential().rotation_keys().is_empty(),
        "expected an empty value"
    );
}

/// On the wire: a route spelling the primary endpoint differently is
/// served with the root key first, not only with a rotation key.
#[tokio::test]
async fn a_respelled_primary_route_presents_the_root_key_with_its_rotation_keys() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let primary = probe.custom_provider();
    let respelled = format!("{primary}/");
    let routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "probe".to_string(),
        provider: respelled,
        model: PROBE_MODEL.to_string(),
        api_key: None,
        credential_ref: None,
    }];

    let config = routed_config(&primary, PROBE_MODEL, Some(ROOT_SENTINEL), None, routes);
    let provider =
        create_provider_from_test_config(&config, &isolated_options(&temp)).expect("test: routed chain builds");
    let _reply = provider.simple_chat("ping", "hint:probe", 0.0).await;

    let requests = probe.requests();
    assert_eq!(
        requests.first().and_then(|request| request.authorization()),
        Some(format!("Bearer {ROOT_SENTINEL}").as_str()),
        "the respelled route must present the root key, not a rotation key alone"
    );
}
