#![cfg(test)]

use super::{Config, agent_provider_selection, issue_cli_agent_turn_proof, resolve_agent_model_choice};
use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL, PROBE_REPLY};

const ROOT_SENTINEL: &str = "C0-AGENT-ROOT-SENTINEL";
const ROTATION_SENTINEL: &str = "C0-AGENT-ROTATION-SENTINEL";

fn isolated_config(temp: &tempfile::TempDir) -> Config {
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some("openai".to_string());
    config.default_model = Some("gpt-4o".to_string());
    config.api_key = Some(ROOT_SENTINEL.to_string());
    config.api_url = None;
    config.model_routes = Vec::new();
    config.reliability.fallback_providers = Vec::new();
    config.reliability.api_keys = Vec::new();
    config.reliability.provider_retries = 0;
    config.reliability.provider_backoff_ms = 0;
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = temp.path().join("no-codex-auth.json");
    config
}

/// T1: a keyless `custom:` override with a foreign root key sends no
/// `Authorization` header at all.
#[tokio::test]
async fn a_custom_override_does_not_receive_the_root_key() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start().await;
    let config = isolated_config(&temp);
    let selected = probe.custom_provider();
    let options = crate::providers::provider_runtime_options_from_config(&config);

    let choice = resolve_agent_model_choice(&config, Some(&selected), Some(PROBE_MODEL))
        .expect("test: a keyless custom endpoint is selectable");
    let provider = agent_provider_selection(&choice)
        .build_provider(&config, &options)
        .expect("test: a keyless custom endpoint builds");
    let reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

    assert_eq!(reply.as_deref().ok(), Some(PROBE_REPLY));
    let requests = probe.requests();
    assert_eq!(requests.len(), 1);
    assert!(!probe.saw(ROOT_SENTINEL), "the root key reached a --provider override");
    assert!(requests.iter().all(|request| request.authorization().is_none()));
}

/// T2: a catalog override without a credential of its own is refused
/// before any provider handle exists, and the refusal carries no secret.
#[test]
fn a_catalog_override_without_its_own_credential_is_refused() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let config = isolated_config(&temp);
    let options = crate::providers::provider_runtime_options_from_config(&config);

    let outcome = resolve_agent_model_choice(&config, Some("mistral"), Some("mistral-large-latest"))
        .and_then(|choice| agent_provider_selection(&choice).build_provider(&config, &options));

    let typed = match outcome {
        Ok(_) => panic!("a catalog override must not build on the default provider's key"),
        Err(error) => error,
    };
    assert!(
        typed
            .downcast_ref::<crate::providers::MissingProviderCredential>()
            .is_some(),
        "the refusal must be the typed missing-credential error: {typed:#}"
    );
    let error = format!("{typed:#}");
    assert!(error.contains("missing credential/api key"), "{error}");
    assert!(!error.contains(ROOT_SENTINEL), "{error}");
}

/// T3 (guard): the configured default, in any spelling of its identity,
/// keeps the root key and base URL.
#[test]
fn the_configured_default_keeps_its_connection() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let mut config = isolated_config(&temp);
    config.api_url = Some("https://api.openai.example/v1".to_string());

    for spelling in [None, Some("openai"), Some("OPENAI")] {
        let choice =
            resolve_agent_model_choice(&config, spelling, None).expect("test: the default provider is selectable");
        assert_eq!(
            choice.resolved.credential().api_key(),
            Some(ROOT_SENTINEL),
            "{spelling:?}"
        );
        assert_eq!(
            choice.resolved.credential().api_url(),
            Some("https://api.openai.example/v1"),
            "{spelling:?}"
        );
    }
}

/// Finding 79(a): the construction-time attestation exists to catch a reload
/// that changes the key under the same credential reference.
#[test]
fn a_changed_key_under_the_same_credential_reference_fails_the_attestation() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let mut config = isolated_config(&temp);
    let choice = resolve_agent_model_choice(&config, None, None).expect("test: the default provider is selectable");
    assert!(choice.factory_credential_source_matches(&config));
    config.api_key = Some("C0-AGENT-ROTATED-ROOT".to_string());
    assert!(
        !choice.factory_credential_source_matches(&config),
        "a rotated key under the same reference must not attest"
    );
}

/// T4: a model the selected provider cannot serve is refused before any
/// connection is resolved, as `prx chat` already does.
#[test]
fn an_incompatible_model_is_refused_before_construction() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let config = isolated_config(&temp);

    let error = match resolve_agent_model_choice(&config, Some("mistral"), Some("openai/gpt-4o")) {
        Ok(_) => panic!("an incompatible model must be refused"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("not compatible"), "{error}");
}

/// T5: rotation keys stay with the default provider. The override's first
/// member is throttled, which is what would rotate to a keyed member.
#[tokio::test]
async fn a_custom_override_does_not_receive_the_rotation_keys() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let mut config = isolated_config(&temp);
    config.reliability.api_keys = vec![ROTATION_SENTINEL.to_string()];
    let selected = probe.custom_provider();
    let options = crate::providers::provider_runtime_options_from_config(&config);

    let choice = resolve_agent_model_choice(&config, Some(&selected), Some(PROBE_MODEL))
        .expect("test: a keyless custom endpoint is selectable");
    let provider = agent_provider_selection(&choice)
        .build_provider(&config, &options)
        .expect("test: a keyless custom endpoint builds");
    let _reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

    assert!(!probe.requests().is_empty());
    assert!(
        !probe.saw(ROTATION_SENTINEL),
        "a rotation key reached a --provider override"
    );
    assert!(!probe.saw(ROOT_SENTINEL));
}

/// T6: the root base URL belongs to the default provider too.
#[test]
fn an_override_does_not_inherit_the_root_base_url() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let mut config = isolated_config(&temp);
    config.api_url = Some("http://127.0.0.1:9/v1".to_string());

    let choice = resolve_agent_model_choice(&config, Some("litellm"), Some("probe-model"))
        .expect("test: litellm accepts a bare model id");
    assert_eq!(choice.resolved.credential().api_url(), None);
    assert_eq!(choice.resolved.credential().api_key(), None);
    assert_eq!(
        choice.resolved.credential().reference(),
        &crate::providers::credentials::CredentialRef::ProviderStore,
        "the override may use only its own credential store"
    );
}

fn declare_decision_only(config: &mut Config, provider: &str) {
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"{provider}\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n"
        ))
        .expect("test: declaration"),
    );
}

/// PMM-C-L: `prx agent` (and cron and xin agent jobs, which enter here)
/// resolve the selection through the provider/model resolver: a model the
/// evidence says cannot chat is refused before any provider exists, and
/// nothing is sent.
#[tokio::test]
async fn a_decision_only_model_is_refused_before_construction() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    let selected = probe.custom_provider();
    declare_decision_only(&mut config, &selected);

    let Err(error) = resolve_agent_model_choice(&config, Some(&selected), Some("jev-decision-test")) else {
        panic!("a decision-only model was selected for the agent loop");
    };
    assert!(format!("{error:#}").contains("cannot be used for chat"), "{error:#}");
    assert!(probe.requests().is_empty(), "nothing is sent");
}

/// PMM-C-L: the agent loop builds a routed chain, so every
/// `[[model_routes]]` destination is resolved too, as `prx chat` does.
#[tokio::test]
async fn a_route_to_a_decision_only_model_is_refused() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    let routed = probe.custom_provider();
    declare_decision_only(&mut config, &routed);
    config.model_routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "decide".to_string(),
        provider: routed,
        model: "jev-decision-test".to_string(),
        api_key: None,
        credential_ref: None,
    }];

    let choice = resolve_agent_model_choice(&config, None, None).expect("test: the default model resolves");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = agent_provider_selection(&choice)
        .build_provider(&config, &options)
        .expect("test: the default builds with the refused route retained");
    let error = provider
        .simple_chat("ping", "hint:decide", 0.0)
        .await
        .expect_err("a route to a decision-only model was accepted");
    assert!(format!("{error:#}").contains("hint:decide"), "{error:#}");
    assert!(probe.requests().is_empty(), "nothing is sent");
}

/// PMM-C-L: a `hint:` model is resolved as the route destination it names;
/// an unknown hint is refused.
#[test]
fn a_hint_model_is_resolved_through_its_route() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let mut config = isolated_config(&temp);
    config.model_routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        api_key: None,
        credential_ref: None,
    }];
    resolve_agent_model_choice(&config, None, Some("hint:fast")).expect("test: a configured hint");
    let Err(error) = resolve_agent_model_choice(&config, None, Some("hint:nope")) else {
        panic!("an unknown hint was accepted");
    };
    assert!(format!("{error:#}").contains("hint:nope"), "{error:#}");
}

/// A direct turn and a keyed same-provider route receive distinct resolved
/// chains. The direct request keeps the root credential and can attest it.
#[tokio::test]
async fn direct_turn_keeps_root_credential_beside_a_keyed_route() {
    let temp = tempfile::tempdir().expect("test workspace");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    let selected = probe.custom_provider();
    config.default_provider = Some(selected.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    config.causal_tree.enabled = false;
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: selected.clone(),
        model: PROBE_MODEL.to_string(),
        api_key: Some("PRX-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    let choice = resolve_agent_model_choice(&config, None, None).expect("direct model selected");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = agent_provider_selection(&choice)
        .build_provider(&config, &options)
        .expect("routed provider");
    assert_eq!(
        provider.simple_chat("ping", PROBE_MODEL, 0.0).await.as_deref().ok(),
        Some(PROBE_REPLY)
    );
    assert!(probe.saw(ROOT_SENTINEL), "the direct request lost the root credential");
    assert!(
        !probe.saw("PRX-ROUTE-KEY"),
        "the direct request used the route credential"
    );
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_ok());
}

/// Two hints to one provider keep distinct resolved credentials, including
/// when the second route is selected.
#[tokio::test]
async fn second_same_provider_hint_uses_its_own_wire_key() {
    let temp = tempfile::tempdir().expect("test workspace");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    let selected = probe.custom_provider();
    config.default_provider = Some(selected.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    config.causal_tree.enabled = false;
    for (hint, key) in [("first", "PRX-FIRST-KEY"), ("second", "PRX-SECOND-KEY")] {
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: hint.to_string(),
            provider: selected.clone(),
            model: PROBE_MODEL.to_string(),
            api_key: Some(key.to_string()),
            credential_ref: None,
        });
    }
    let choice = resolve_agent_model_choice(&config, None, Some("hint:second")).expect("second hint selected");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = agent_provider_selection(&choice)
        .build_provider(&config, &options)
        .expect("routed provider");
    assert_eq!(
        provider.simple_chat("ping", "hint:second", 0.0).await.as_deref().ok(),
        Some(PROBE_REPLY)
    );
    assert!(probe.saw("PRX-SECOND-KEY"), "the second route lost its credential");
    assert!(
        !probe.saw("PRX-FIRST-KEY"),
        "the second route shared the first route's credential"
    );
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_ok());
}

#[tokio::test]
async fn keyed_hint_after_unkeyed_route_uses_its_own_key() {
    let temp = tempfile::tempdir().expect("test workspace");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    let selected = probe.custom_provider();
    config.default_provider = Some(selected.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    config.causal_tree.enabled = false;
    config.model_routes = vec![
        crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "unkeyed".to_string(),
            provider: selected.clone(),
            model: PROBE_MODEL.to_string(),
            api_key: None,
            credential_ref: None,
        },
        crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "keyed".to_string(),
            provider: selected,
            model: PROBE_MODEL.to_string(),
            api_key: Some("PRX-LATER-KEY".to_string()),
            credential_ref: None,
        },
    ];
    let choice = resolve_agent_model_choice(&config, None, Some("hint:keyed")).expect("keyed hint selected");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = agent_provider_selection(&choice)
        .build_provider(&config, &options)
        .expect("routed provider");
    assert_eq!(
        provider.simple_chat("ping", "hint:keyed", 0.0).await.as_deref().ok(),
        Some(PROBE_REPLY)
    );
    assert!(
        probe.saw("PRX-LATER-KEY"),
        "the selected keyed route lost its credential"
    );
    assert!(
        !probe.saw(ROOT_SENTINEL),
        "the selected route inherited the root credential"
    );
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_ok());
}

#[tokio::test]
async fn keyed_primary_hint_retries_without_root_rotation_key() {
    let temp = tempfile::tempdir().expect("test workspace");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let mut config = isolated_config(&temp);
    let selected = probe.custom_provider();
    config.default_provider = Some(selected.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    config.causal_tree.enabled = false;
    config.reliability.api_keys = vec![ROTATION_SENTINEL.to_string()];
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: selected,
        model: PROBE_MODEL.to_string(),
        api_key: Some("PRX-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    let choice = resolve_agent_model_choice(&config, None, Some("hint:fast")).expect("keyed hint selected");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = agent_provider_selection(&choice)
        .build_provider(&config, &options)
        .expect("routed provider");
    let _ = provider.simple_chat("ping", "hint:fast", 0.0).await;
    assert!(probe.saw("PRX-ROUTE-KEY"));
    assert!(!probe.saw(ROTATION_SENTINEL), "retry exposed the root rotation key");
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_ok());
}

#[test]
fn keyed_primary_hint_does_not_inherit_root_endpoint() {
    let temp = tempfile::tempdir().expect("test workspace");
    let mut config = isolated_config(&temp);
    config.api_url = Some("http://127.0.0.1:9/v1".to_string());
    config.causal_tree.enabled = false;
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        api_key: Some("PRX-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    let choice = resolve_agent_model_choice(&config, None, Some("hint:fast")).expect("keyed hint selected");
    let selection = agent_provider_selection(&choice);
    assert_eq!(selection.resolved.credential().api_url(), None);
    selection
        .build_provider(
            &config,
            &crate::providers::provider_runtime_options_from_config(&config),
        )
        .expect("the route builds from its resolved key without the root URL");
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_ok());
}

#[test]
fn raw_provider_whitespace_cannot_hide_a_keyed_factory_route() {
    let temp = tempfile::tempdir().expect("test workspace");
    let mut config = isolated_config(&temp);
    config.default_provider = Some("openai ".to_string());
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "openai ".to_string(),
        model: "gpt-4o-mini".to_string(),
        api_key: Some("PRX-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    let choice = resolve_agent_model_choice(&config, None, None).expect("PMM trims the selected provider label");
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_err());
}

#[tokio::test]
async fn root_url_without_root_key_uses_the_provider_profile() {
    let temp = tempfile::tempdir().expect("test workspace");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    config.api_key = None;
    config.api_url = Some(
        probe
            .custom_provider()
            .strip_prefix("custom:")
            .expect("loopback provider URL")
            .to_string(),
    );
    config.causal_tree.enabled = false;
    config.secrets.encrypt = false;
    let store = crate::auth::profiles::AuthProfilesStore::new(temp.path(), false);
    store
        .upsert_profile(
            crate::auth::profiles::AuthProfile::new_token("openai", "default", "PRX-PROFILE-KEY".to_string()),
            true,
        )
        .expect("isolated profile");
    let choice = resolve_agent_model_choice(&config, None, None).expect("PMM root URL selection");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let (built, counts) = crate::providers::credential_resolution_observation::observe(|| {
        agent_provider_selection(&choice).build_provider(&config, &options)
    });
    let provider = built.expect("URL-only Root falls back to the provider's own credential store");
    assert_eq!(
        counts,
        [1, 1, 0],
        "construction must perform one provider-profile lookup"
    );
    let reply = provider
        .simple_chat("ping", "gpt-4o", 0.0)
        .await
        .expect("the restored credential reaches the configured Root URL");
    assert_eq!(reply, PROBE_REPLY);
    assert!(probe.saw("PRX-PROFILE-KEY"), "the provider profile was not used");
    assert!(choice.factory_credential_source_matches(&config));
}

#[test]
fn openai_codex_ignores_root_key_url_and_rotation_material() {
    let temp = tempfile::tempdir().expect("test workspace");
    let mut config = isolated_config(&temp);
    config.default_provider = Some("openai-codex".to_string());
    config.default_model = Some("gpt-5.2-codex".to_string());
    config.api_key = Some("PRX-IGNORED-CODEX-ROOT".to_string());
    config.api_url = Some("https://ignored.example.invalid/v1".to_string());
    config.reliability.api_keys = vec!["PRX-IGNORED-CODEX-ROTATION".to_string()];
    let choice = resolve_agent_model_choice(&config, None, None).expect("Codex Root selection resolves");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let (provider, counts) = crate::providers::credential_resolution_observation::observe(|| {
        agent_provider_selection(&choice).build_provider(&config, &options)
    });
    provider.expect("Codex builds from its own login despite unrelated Root material");
    assert_eq!(counts, [0, 0, 0], "Codex must not resolve Root API-key material");
}

/// Kimi Code accepts any model string and answers anyway, so an id outside
/// the catalog allowlist is refused locally; that stays true through the resolver.
#[test]
fn an_unlisted_kimi_code_model_is_still_refused() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let config = isolated_config(&temp);
    let Err(error) = resolve_agent_model_choice(&config, Some("kimi-code"), Some("definitely-not-a-model")) else {
        panic!("an unlisted kimi-code model was accepted");
    };
    assert!(format!("{error:#}").contains("kimi-code"), "{error:#}");
    resolve_agent_model_choice(&config, Some("kimi-code"), Some("k3")).expect("test: a listed model");
    resolve_agent_model_choice(&config, Some("kimi-code"), Some("k3.1-pro")).expect("test: catalog active id");
}

/// Real wire plus actual factory lookup observation: no header-only inference.
#[tokio::test]
async fn resolved_url_only_roots_restore_provider_profile_lookup_on_owned_wire() {
    use futures_util::FutureExt;
    use parking_lot::Mutex;
    use std::sync::Arc;
    struct Server(tokio::task::JoinHandle<std::io::Result<()>>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    for label in ["openai", "ollama", "custom"] {
        let temp = tempfile::tempdir().expect("owned configuration");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("owned listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("listener address"));
        let handler = axum::routing::post(move |headers: axum::http::HeaderMap| {
            let capture = Arc::clone(&capture);
            async move {
                capture.lock().push(
                    headers
                        .get(axum::http::header::AUTHORIZATION)
                        .map(|value| value.to_str().expect("neutral header").to_owned()),
                );
                axum::Json(serde_json::json!({
                    "model": "prx-local-model", "done": true,
                    "message": {"role":"assistant", "content":"PRX keyless reply"},
                    "choices":[{"index":0,"message":{"role":"assistant","content":"PRX keyless reply"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                }))
            }
        });
        let app = axum::Router::new()
            .route("/api/chat", handler.clone())
            .route("/chat/completions", handler.clone())
            .route("/v1/chat/completions", handler);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let mut server = Server(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        }));
        let outcome = std::panic::AssertUnwindSafe(async {
            let mut config = isolated_config(&temp);
            config.causal_tree.enabled = false;
            config.api_key = None;
            config.api_url = Some(endpoint.clone());
            let provider_label = if label == "custom" {
                format!("custom:{endpoint}/v1")
            } else {
                label.to_owned()
            };
            config.default_provider = Some(provider_label.clone());
            config.default_model = Some("prx-local-model".to_owned());
            config.secrets.encrypt = false;
            let store = crate::auth::profiles::AuthProfilesStore::new(temp.path(), false);
            store
                .upsert_profile(
                    crate::auth::profiles::AuthProfile::new_token(
                        &provider_label,
                        "default",
                        "PRX-CONFLICTING-PROFILE".into(),
                    ),
                    true,
                )
                .expect("owned conflicting profile");
            let choice = resolve_agent_model_choice(&config, None, None).expect("actual resolved connection");
            let options = crate::providers::provider_runtime_options_from_config(&config);
            let selection = agent_provider_selection(&choice);
            let mut changed = config.clone();
            changed.model_routes.push(crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "prx_route".into(),
                provider: provider_label.clone(),
                model: "prx-local-model".into(),
                api_key: Some("PRX-CHANGED-ROUTE".into()),
                credential_ref: None,
            });
            let (built, counts) = crate::providers::credential_resolution_observation::observe(|| {
                selection.build_provider(&changed, &options)
            });
            assert!(
                built.is_ok(),
                "an unrelated route cannot invalidate the resolved default"
            );
            assert_eq!(
                counts,
                [2, 1, 0],
                "the URL-only Root resolves its provider profile and the new route resolves its explicit key"
            );
            assert!(seen.lock().is_empty());
            let (provider, counts) = crate::providers::credential_resolution_observation::observe(|| {
                agent_provider_selection(&choice).build_provider(&config, &options)
            });
            let provider = provider.expect("actual provider construction");
            let reply = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                provider.simple_chat("ping", "prx-local-model", 0.0),
            )
            .await
            .expect("bounded wire")
            .expect("actual reply");
            assert_eq!(reply, "PRX keyless reply");
            {
                let requests = seen.lock();
                assert_eq!(requests.len(), 1);
                assert_eq!(counts, [1, 1, 0]);
                let expected_authorization = (label != "ollama").then_some("Bearer PRX-CONFLICTING-PROFILE");
                assert_eq!(
                    requests.first().and_then(Option::as_deref),
                    expected_authorization,
                    "{label} changed how it presents a resolved provider credential"
                );
            }
            // The typed PMM factory must preserve the same absence independently
            // of the CLI routed entry.
            let catalog = crate::providers::selection::catalog_for(&config);
            let (typed, typed_counts) = crate::providers::credential_resolution_observation::observe(|| {
                crate::providers::create_provider_for(
                    &choice.resolved,
                    &catalog,
                    &config.reliability,
                    &config.cost,
                    &options,
                )
            });
            let typed = typed.expect("typed resolved construction");
            let reply = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                typed.simple_chat("ping", "prx-local-model", 0.0),
            )
            .await
            .expect("bounded typed wire")
            .expect("typed reply");
            assert_eq!(reply, "PRX keyless reply");
            {
                let requests = seen.lock();
                assert_eq!(requests.len(), 2);
                assert_eq!(typed_counts, [1, 1, 0]);
                let expected_authorization = (label != "ollama").then_some("Bearer PRX-CONFLICTING-PROFILE");
                assert!(
                    requests
                        .iter()
                        .all(|authorization| authorization.as_deref() == expected_authorization),
                    "{label} changed how it presents a resolved provider credential"
                );
            }
            let generation = crate::config::new_shared(config).pin();
            let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
                .with_config_generation(&generation);
            let proof = issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice);
            assert!(proof.is_ok());
        })
        .catch_unwind()
        .await;
        let _ = stop.send(());
        let completion = tokio::time::timeout(std::time::Duration::from_secs(5), &mut server.0).await;
        if completion.is_err() {
            server.0.abort();
            let _ = (&mut server.0).await;
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
        completion
            .expect("server drain deadline")
            .expect("server task")
            .expect("server result");
    }
}
