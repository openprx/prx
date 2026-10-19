#![cfg(test)]

use super::{chat_model_switch, chat_provider_chain};
use crate::config::Config;
use crate::providers::configured_connection_for as configured_provider_connection;
use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};

const ROOT_SENTINEL: &str = "C0-CHAT-ROOT-SENTINEL";
const ROTATION_SENTINEL: &str = "C0-CHAT-ROTATION-SENTINEL";

fn isolated_config(temp: &tempfile::TempDir) -> Config {
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some("openai".to_string());
    config.api_key = Some(ROOT_SENTINEL.to_string());
    config.api_url = None;
    config.model_routes = Vec::new();
    config.reliability.fallback_providers = Vec::new();
    config.reliability.api_keys = vec![ROTATION_SENTINEL.to_string()];
    config.reliability.provider_retries = 0;
    config.reliability.provider_backoff_ms = 0;
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = temp.path().join("no-codex-auth.json");
    config
}

fn declare_decision_only(config: &mut Config, provider: &str) {
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"{provider}\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n"
        ))
        .expect("test: declaration"),
    );
}

/// PMM-C: an alias of the configured provider is that provider, so it
/// keeps the root key (chat used to compare spellings).
#[test]
fn an_alias_of_the_configured_provider_keeps_its_connection() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let mut config = isolated_config(&temp);
    config.default_provider = Some("anthropic".to_string());
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let (connection, _provider) = chat_provider_chain(&config, "claude-code", "anthropic", "claude-sonnet-4", &options)
        .expect("test: the alias builds");
    assert!(connection.released, "claude-code is anthropic");
    assert_eq!(connection.api_key, Some(ROOT_SENTINEL));
}

/// PMM-C: `prx chat -p` and `/provider` refuse a model that cannot chat
/// before any provider exists, and `/model` refuses it on the current
/// provider.
#[tokio::test]
async fn a_decision_only_model_is_refused_by_every_chat_switch() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start().await;
    let mut config = isolated_config(&temp);
    declare_decision_only(&mut config, &probe.custom_provider());
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let Err(error) = chat_provider_chain(
        &config,
        &probe.custom_provider(),
        "openai",
        "jev-decision-test",
        &options,
    ) else {
        panic!("a decision-only model built the chat chain");
    };
    assert!(format!("{error:#}").contains("cannot be used for chat"), "{error:#}");
    let error = chat_model_switch(&config, &probe.custom_provider(), "jev-decision-test")
        .expect_err("`/model` accepted a decision-only model");
    assert!(format!("{error:#}").contains("cannot be used for chat"), "{error:#}");
    assert!(probe.requests().is_empty(), "nothing is sent");
    chat_model_switch(&config, &probe.custom_provider(), "a-chat-model").expect("an id-only model still switches");
}

/// `prx chat -p <other>` and `/provider <other>`: the rotation keys stay
/// with the configured provider. The endpoint throttles its first request,
/// which is when a chain rotates to its next keyed member.
#[tokio::test]
async fn a_chat_provider_override_does_not_receive_the_rotation_keys() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let config = isolated_config(&temp);
    let options = crate::providers::provider_runtime_options_from_config(&config);

    let (connection, provider) =
        chat_provider_chain(&config, &probe.custom_provider(), "openai", PROBE_MODEL, &options)
            .expect("test: a keyless custom endpoint builds");
    let _reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

    assert_eq!(connection.api_key, None);
    assert!(
        !connection.released,
        "an override cannot grant root connection provenance"
    );
    assert!(!probe.requests().is_empty());
    assert!(
        !probe.saw(ROTATION_SENTINEL),
        "a rotation key reached a chat provider override"
    );
    assert!(!probe.saw(ROOT_SENTINEL));
}

/// Guard: the configured provider keeps its key and its rotation keys.
#[tokio::test]
async fn the_configured_chat_provider_keeps_its_rotation_keys() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let mut config = isolated_config(&temp);
    let configured = probe.custom_provider();
    config.default_provider = Some(configured.clone());
    let options = crate::providers::provider_runtime_options_from_config(&config);

    let (_connection, provider) = chat_provider_chain(&config, &configured, &configured, PROBE_MODEL, &options)
        .expect("test: the configured endpoint builds");
    let reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

    assert!(reply.is_ok(), "the rotated member answers: {reply:?}");
    assert!(probe.saw(ROOT_SENTINEL));
    assert!(probe.saw(ROTATION_SENTINEL));
}

#[test]
fn cli_provider_override_does_not_inherit_default_provider_connection() {
    assert_eq!(
        configured_provider_connection(
            "kimi-code",
            "custom:http://127.0.0.1:18083/v1",
            Some("local-placeholder"),
            Some("http://127.0.0.1:18083/v1"),
        ),
        crate::providers::ProviderConnection {
            api_key: None,
            api_url: None,
            released: false,
        }
    );
}

#[test]
fn configured_provider_keeps_its_connection() {
    assert_eq!(
        configured_provider_connection(
            "CUSTOM:http://127.0.0.1:18083/v1",
            "custom:http://127.0.0.1:18083/v1",
            Some("local-placeholder"),
            Some("http://127.0.0.1:18083/v1"),
        ),
        crate::providers::ProviderConnection {
            api_key: Some("local-placeholder"),
            api_url: Some("http://127.0.0.1:18083/v1"),
            released: true,
        }
    );
}
