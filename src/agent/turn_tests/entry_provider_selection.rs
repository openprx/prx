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
    let provider = agent_provider_selection(&config, &choice)
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
        .and_then(|choice| agent_provider_selection(&config, &choice).build_provider(&config, &options));

    let error = match outcome {
        Ok(_) => panic!("a catalog override must not build on the default provider's key"),
        Err(error) => format!("{error:#}"),
    };
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
        let selection = agent_provider_selection(&config, &choice);
        assert_eq!(selection.connection.api_key, Some(ROOT_SENTINEL), "{spelling:?}");
        assert_eq!(
            selection.connection.api_url,
            Some("https://api.openai.example/v1"),
            "{spelling:?}"
        );
    }
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
    let provider = agent_provider_selection(&config, &choice)
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
    let selection = agent_provider_selection(&config, &choice);

    assert_eq!(selection.connection.api_url, None);
    assert_eq!(selection.connection.api_key, None);
    assert!(!selection.connection.released);
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
        hint: "decide".to_string(),
        provider: routed,
        model: "jev-decision-test".to_string(),
        api_key: None,
        credential_ref: None,
    }];

    let Err(error) = resolve_agent_model_choice(&config, None, None) else {
        panic!("a route to a decision-only model was accepted");
    };
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

/// The current routed factory shares one handle per provider name. A direct
/// turn with a keyed same-provider route therefore cannot issue a decision
/// proof that claims the root credential reached the wire.
#[tokio::test]
async fn direct_turn_refuses_proof_when_factory_uses_a_route_key() {
    let temp = tempfile::tempdir().expect("test workspace");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    let selected = probe.custom_provider();
    config.default_provider = Some(selected.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: selected.clone(),
        model: PROBE_MODEL.to_string(),
        api_key: Some("PRX-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    let choice = resolve_agent_model_choice(&config, None, None).expect("direct model selected");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = agent_provider_selection(&config, &choice)
        .build_provider(&config, &options)
        .expect("routed provider");
    assert_eq!(
        provider.simple_chat("ping", PROBE_MODEL, 0.0).await.as_deref().ok(),
        Some(PROBE_REPLY)
    );
    assert!(probe.saw("PRX-ROUTE-KEY"), "the factory used its keyed route");
    assert!(!probe.saw(ROOT_SENTINEL));
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(
        issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_err(),
        "a root credential proof cannot attest to a route-key wire request"
    );
}

/// Two hints to one provider with different keys share the first routed
/// provider. The selected second hint cannot issue a credential proof.
#[tokio::test]
async fn second_same_provider_hint_refuses_first_hint_wire_key() {
    let temp = tempfile::tempdir().expect("test workspace");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    let selected = probe.custom_provider();
    config.default_provider = Some(selected.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    for (hint, key) in [("first", "PRX-FIRST-KEY"), ("second", "PRX-SECOND-KEY")] {
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: hint.to_string(),
            provider: selected.clone(),
            model: PROBE_MODEL.to_string(),
            api_key: Some(key.to_string()),
            credential_ref: None,
        });
    }
    let choice = resolve_agent_model_choice(&config, None, Some("hint:second")).expect("second hint selected");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = agent_provider_selection(&config, &choice)
        .build_provider(&config, &options)
        .expect("routed provider");
    assert_eq!(
        provider.simple_chat("ping", "hint:second", 0.0).await.as_deref().ok(),
        Some(PROBE_REPLY)
    );
    assert!(probe.saw("PRX-FIRST-KEY"), "the factory shared its first keyed route");
    assert!(!probe.saw("PRX-SECOND-KEY"));
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(
        issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_err(),
        "the second hint cannot attest to the first hint's wire credential"
    );
}

#[tokio::test]
async fn keyed_hint_after_unkeyed_route_refuses_factory_fallback_key() {
    let temp = tempfile::tempdir().expect("test workspace");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    let selected = probe.custom_provider();
    config.default_provider = Some(selected.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    config.model_routes = vec![
        crate::config::ModelRouteConfig {
            hint: "unkeyed".to_string(),
            provider: selected.clone(),
            model: PROBE_MODEL.to_string(),
            api_key: None,
            credential_ref: None,
        },
        crate::config::ModelRouteConfig {
            hint: "keyed".to_string(),
            provider: selected,
            model: PROBE_MODEL.to_string(),
            api_key: Some("PRX-LATER-KEY".to_string()),
            credential_ref: None,
        },
    ];
    let choice = resolve_agent_model_choice(&config, None, Some("hint:keyed")).expect("keyed hint selected");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = agent_provider_selection(&config, &choice)
        .build_provider(&config, &options)
        .expect("routed provider");
    assert_eq!(
        provider.simple_chat("ping", "hint:keyed", 0.0).await.as_deref().ok(),
        Some(PROBE_REPLY)
    );
    assert!(probe.saw(ROOT_SENTINEL), "the first unkeyed route falls back to root");
    assert!(!probe.saw("PRX-LATER-KEY"));
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_err());
}

#[tokio::test]
async fn keyed_primary_hint_refuses_root_rotation_on_retry() {
    let temp = tempfile::tempdir().expect("test workspace");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let mut config = isolated_config(&temp);
    let selected = probe.custom_provider();
    config.default_provider = Some(selected.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    config.reliability.api_keys = vec![ROTATION_SENTINEL.to_string()];
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: selected,
        model: PROBE_MODEL.to_string(),
        api_key: Some("PRX-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    let choice = resolve_agent_model_choice(&config, None, Some("hint:fast")).expect("keyed hint selected");
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = agent_provider_selection(&config, &choice)
        .build_provider(&config, &options)
        .expect("routed provider");
    let _ = provider.simple_chat("ping", "hint:fast", 0.0).await;
    assert!(probe.saw("PRX-ROUTE-KEY"));
    assert!(probe.saw(ROTATION_SENTINEL), "retry exposed the root rotation key");
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_err());
}

#[test]
fn keyed_primary_hint_refuses_root_endpoint_mix() {
    let temp = tempfile::tempdir().expect("test workspace");
    let mut config = isolated_config(&temp);
    config.api_url = Some("http://127.0.0.1:9/v1".to_string());
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        api_key: Some("PRX-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    let choice = resolve_agent_model_choice(&config, None, Some("hint:fast")).expect("keyed hint selected");
    let selection = agent_provider_selection(&config, &choice);
    assert_eq!(selection.connection.api_url, Some("http://127.0.0.1:9/v1"));
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_err());
}

#[test]
fn raw_provider_whitespace_cannot_hide_a_keyed_factory_route() {
    let temp = tempfile::tempdir().expect("test workspace");
    let mut config = isolated_config(&temp);
    config.default_provider = Some("openai ".to_string());
    config.model_routes.push(crate::config::ModelRouteConfig {
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
async fn root_url_without_root_key_cannot_attest_provider_profile_wire_key() {
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
    let provider = agent_provider_selection(&config, &choice)
        .build_provider(&config, &options)
        .expect("profile-backed provider");
    assert_eq!(
        provider.simple_chat("ping", "gpt-4o", 0.0).await.as_deref().ok(),
        Some(PROBE_REPLY)
    );
    assert!(
        probe.saw("PRX-PROFILE-KEY"),
        "factory used provider store rather than Root"
    );
    let generation = crate::config::new_shared(config).pin();
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
        .with_config_generation(&generation);
    assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_err());
}

/// Kimi Code accepts any model string and answers anyway, so an id outside
/// its short list is refused locally; that stays true through the resolver.
#[test]
fn an_unlisted_kimi_code_model_is_still_refused() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let config = isolated_config(&temp);
    let Err(error) = resolve_agent_model_choice(&config, Some("kimi-code"), Some("definitely-not-a-model")) else {
        panic!("an unlisted kimi-code model was accepted");
    };
    assert!(format!("{error:#}").contains("kimi-code"), "{error:#}");
    resolve_agent_model_choice(&config, Some("kimi-code"), Some("k3")).expect("test: a listed model");
}

/// Real wire plus actual factory lookup observation: no header-only inference.
#[tokio::test]
async fn pinned_keyless_root_skips_profile_resolution_on_owned_wire() {
    use futures_util::FutureExt;
    use parking_lot::Mutex;
    use std::sync::Arc;
    struct Server(tokio::task::JoinHandle<std::io::Result<()>>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    for label in ["ollama", "custom", "openai"] {
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
            if label != "openai" {
                let selection = agent_provider_selection(&config, &choice);
                let mut changed = config.clone();
                changed.model_routes.push(crate::config::ModelRouteConfig {
                    hint: "prx_route".into(),
                    provider: provider_label.clone(),
                    model: "prx-local-model".into(),
                    api_key: Some("PRX-CHANGED-ROUTE".into()),
                    credential_ref: None,
                });
                let (refused, counts) = crate::providers::credential_resolution_observation::observe(|| {
                    selection.build_provider(&changed, &options)
                });
                assert!(refused.is_err());
                assert_eq!(counts, [0, 0, 0]);
                assert!(seen.lock().is_empty());
            }
            let (provider, counts) = crate::providers::credential_resolution_observation::observe(|| {
                agent_provider_selection(&config, &choice).build_provider(&config, &options)
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
                if label == "openai" {
                    assert!(counts.first().is_some_and(|count| *count > 0));
                    assert!(counts.get(1).is_some_and(|count| *count > 0));
                    assert_eq!(
                        requests.first().and_then(Option::as_deref),
                        Some("Bearer PRX-CONFLICTING-PROFILE")
                    );
                } else {
                    assert_eq!(counts, [0, 0, 0]);
                    assert!(requests.first().is_some_and(Option::is_none));
                }
            }
            // The typed PMM factory must preserve the same absence independently
            // of the CLI routed entry, with the same non-vacuous legacy control.
            let catalog = crate::providers::selection::catalog_for(&config);
            let (typed, typed_counts) = crate::providers::credential_resolution_observation::observe(|| {
                crate::providers::create_provider_for(&choice.resolved, &catalog, &config.reliability, &options)
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
                if label == "openai" {
                    assert!(typed_counts.first().is_some_and(|count| *count > 0));
                    assert!(typed_counts.get(1).is_some_and(|count| *count > 0));
                    assert!(
                        requests
                            .iter()
                            .all(|header| header.as_deref() == Some("Bearer PRX-CONFLICTING-PROFILE"))
                    );
                } else {
                    assert_eq!(typed_counts, [0, 0, 0]);
                    assert!(requests.iter().all(Option::is_none));
                }
            }
            let generation = crate::config::new_shared(config).pin();
            let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(temp.path().to_string_lossy(), "prx_run")
                .with_config_generation(&generation);
            let proof = issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice);
            assert_eq!(proof.is_ok(), label != "openai");
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
