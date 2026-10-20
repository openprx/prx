#![cfg(test)]

use super::base_manifest;
use crate::channels::traits::ChannelMessage;
use crate::config::Config;
use crate::memory::{Memory, MemoryFabric};
use crate::providers::Provider;
use crate::security::SecurityPolicy;
use crate::session_worker::protocol::WorkerManifest;
use crate::session_worker::runner::capability::session_secret_path;
use crate::session_worker::runner::context::boxed_worker_segments;
use crate::session_worker::runner::provider::{
    validate_worker_route, worker_credentials, worker_provider, worker_spawn_tool,
};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[test]
fn worker_keeps_gateway_credentials_out_of_an_overridden_provider() {
    let mut manifest = base_manifest(Path::new("/tmp/worker-credentials"), "capability");
    let mut config = Config::default();
    config.default_provider = Some("qwen-gateway".to_string());
    config.api_key = Some("gateway-key".to_string());
    config.api_url = Some("http://gateway.invalid/v1".to_string());

    manifest.provider_name = "kimi-code".to_string();
    let connection = worker_credentials(&manifest, &config).provider;
    assert_eq!(connection.api_key, None);
    assert_eq!(connection.api_url, None);

    manifest.api_key = Some("kimi-key".to_string());
    let connection = worker_credentials(&manifest, &config).provider;
    assert_eq!(connection.api_key, Some("kimi-key"));
    assert_eq!(connection.api_url, None);

    manifest.provider_name = "qwen-gateway".to_string();
    manifest.api_key = Some("gateway-key".to_string());
    manifest.direct_uses_root_connection = true;
    validate_worker_route(&manifest, &config).expect("root manifest matches config");
    let connection = worker_credentials(&manifest, &config).provider;
    assert_eq!(connection.api_key, Some("gateway-key"));
    assert_eq!(connection.api_url, Some("http://gateway.invalid/v1"));
}

#[test]
fn direct_agent_key_equal_to_root_bytes_has_no_root_connection() {
    let mut manifest = base_manifest(Path::new("/tmp/worker-direct-provenance"), "capability");
    let mut config = Config::default();
    config.default_provider = Some("openai".to_string());
    config.api_key = Some("PRX-SAME-KEY".to_string());
    config.api_url = Some("http://root.invalid/v1".to_string());
    config.reliability.api_keys = vec!["PRX-ROOT-ROTATION".to_string()];
    config.agents.insert(
        "prx_agent".to_string(),
        toml::from_str("provider = 'openai'\nmodel = 'gpt-4o'\napi_key = 'PRX-SAME-KEY'\n").expect("agent fixture"),
    );
    manifest.provider_name = "openai".to_string();
    manifest.model = "gpt-4o".to_string();
    manifest.api_key = Some("PRX-SAME-KEY".to_string());
    manifest.agent_id = Some("prx_agent".to_string());
    manifest.direct_uses_root_connection = false;
    validate_worker_route(&manifest, &config).expect("agent-owned key remains non-root");
    let credentials = worker_credentials(&manifest, &config);
    assert_eq!(credentials.provider.api_key, Some("PRX-SAME-KEY"));
    assert_eq!(credentials.provider.api_url, None);
    assert!(credentials.reliability.api_keys.is_empty(), "expected an empty value");

    manifest.direct_uses_root_connection = true;
    assert!(
        validate_worker_route(&manifest, &config).is_err(),
        "agent cannot claim root connection"
    );
}

#[test]
fn selected_route_key_equal_to_root_key_still_gets_no_root_url_or_rotation_keys() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let mut config = root_config(temp.path());
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        api_key: config.api_key.clone(),
        credential_ref: None,
    });
    let mut manifest = base_manifest(temp.path(), "capability");
    manifest.provider_name = "openai".to_string();
    manifest.model = "gpt-4o-mini".to_string();
    manifest.api_key = config.api_key.clone();
    manifest.route_hint = Some("fast".to_string());
    manifest.route_source_provider = Some("openai".to_string());
    manifest.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
        crate::providers::credentials::LegacySlot::ModelRoute("fast".to_string()),
    ));
    validate_worker_route(&manifest, &config).expect("sealed route matches");
    let credentials = worker_credentials(&manifest, &config);
    assert_eq!(credentials.provider.api_key, Some(WORKER_ROOT_SENTINEL));
    assert_eq!(credentials.provider.api_url, None);
    assert!(credentials.reliability.api_keys.is_empty(), "expected an empty value");

    manifest.route_uses_root_connection = true;
    assert!(validate_worker_route(&manifest, &config).is_err());
    manifest.route_uses_root_connection = false;
    manifest.model = "different-model".to_string();
    assert!(validate_worker_route(&manifest, &config).is_err());
}

#[test]
fn sealed_foreign_route_refuses_root_key_and_config_drift() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let mut config = root_config(temp.path());
    let destination = "custom:http://127.0.0.1:18081/v1";
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: destination.to_string(),
        model: "probe-model".to_string(),
        api_key: Some("PRX-WORKER-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    let mut manifest = base_manifest(temp.path(), "capability");
    manifest.provider_name = destination.to_string();
    manifest.model = "probe-model".to_string();
    manifest.api_key = Some("PRX-WORKER-ROUTE-KEY".to_string());
    manifest.route_hint = Some("fast".to_string());
    manifest.route_source_provider = Some("openai".to_string());
    manifest.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
        crate::providers::credentials::LegacySlot::ModelRoute("fast".to_string()),
    ));
    validate_worker_route(&manifest, &config).expect("selected route matches sealed config");
    let credentials = worker_credentials(&manifest, &config);
    assert_eq!(credentials.provider.api_key, Some("PRX-WORKER-ROUTE-KEY"));
    assert_eq!(credentials.provider.api_url, None);
    assert_eq!(credentials.spawn_fallback, config.api_key.as_deref());

    manifest.api_key = config.api_key.clone();
    assert!(
        validate_worker_route(&manifest, &config).is_err(),
        "root key cannot become route key"
    );
    manifest.api_key = Some("PRX-WORKER-ROUTE-KEY".to_string());
    manifest.route_uses_root_connection = true;
    assert!(
        validate_worker_route(&manifest, &config).is_err(),
        "foreign route cannot use root URL"
    );
    manifest.route_uses_root_connection = false;
    config.model_routes.get_mut(0).expect("route fixture").model = "changed-model".to_string();
    assert!(
        validate_worker_route(&manifest, &config).is_err(),
        "route target drift is refused"
    );
    config.model_routes.get_mut(0).expect("route fixture").model = "probe-model".to_string();
    manifest.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
        crate::providers::credentials::LegacySlot::Agent("missing-agent".to_string()),
    ));
    assert!(
        validate_worker_route(&manifest, &config).is_err(),
        "tampered credential source is refused"
    );
}

#[test]
fn agent_profile_route_keeps_credential_source_without_execution_identity() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let mut config = root_config(temp.path());
    let destination = "custom:http://127.0.0.1:18082/v1";
    config.secrets.encrypt = false;
    let profile = crate::auth::AuthService::new(temp.path(), false)
        .store_provider_token(
            destination,
            "agent-route",
            "PRX-WORKER-AGENT-PROFILE",
            std::collections::HashMap::new(),
            false,
        )
        .expect("test: agent profile");
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "agent".to_string(),
        provider: destination.to_string(),
        model: "probe-model".to_string(),
        api_key: None,
        credential_ref: None,
    });
    let mut manifest = base_manifest(temp.path(), "capability");
    manifest.provider_name = destination.to_string();
    manifest.model = "probe-model".to_string();
    manifest.api_key = Some("PRX-WORKER-AGENT-PROFILE".to_string());
    manifest.route_hint = Some("agent".to_string());
    manifest.route_source_provider = Some(destination.to_string());
    manifest.route_credential = Some(crate::providers::credentials::CredentialRef::Profile { id: profile.id });
    manifest.agent_id = None;
    validate_worker_route(&manifest, &config).expect("sealed profile works independently of execution identity");
    let credentials = worker_credentials(&manifest, &config);
    assert_eq!(credentials.provider.api_key, Some("PRX-WORKER-AGENT-PROFILE"));
    assert_eq!(credentials.provider.api_url, None);
    assert_eq!(credentials.spawn_fallback, config.api_key.as_deref());

    manifest.api_key = config.api_key.clone();
    assert!(
        validate_worker_route(&manifest, &config).is_err(),
        "root key is not agent profile"
    );
    manifest.api_key = Some("PRX-WORKER-AGENT-PROFILE".to_string());
    manifest.route_source_provider = Some("openai".to_string());
    assert!(
        validate_worker_route(&manifest, &config).is_err(),
        "source substitution is refused"
    );
    manifest.route_source_provider = Some(destination.to_string());
    manifest.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
        crate::providers::credentials::LegacySlot::Root,
    ));
    assert!(
        validate_worker_route(&manifest, &config).is_err(),
        "reference substitution is refused"
    );
}

const WORKER_ROOT_SENTINEL: &str = "C0-WORKER-ROOT-SENTINEL";

const WORKER_ROTATION_SENTINEL: &str = "C0-WORKER-ROTATION-SENTINEL";

const WORKER_AGENT_SENTINEL: &str = "C0-WORKER-AGENT-SENTINEL";

fn root_config(temp: &Path) -> Config {
    let mut config = Config::default();
    config.config_path = temp.join("config.toml");
    config.workspace_dir = temp.to_path_buf();
    config.default_provider = Some("openai".to_string());
    config.api_key = Some(WORKER_ROOT_SENTINEL.to_string());
    config.api_url = Some("https://api.openai.example/v1".to_string());
    config.reliability.api_keys = vec![WORKER_ROTATION_SENTINEL.to_string()];
    config.reliability.fallback_providers = Vec::new();
    config.reliability.provider_retries = 0;
    config.reliability.provider_backoff_ms = 0;
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = temp.join("no-codex-auth.json");
    config
}

fn non_default_manifest(temp: &Path, api_key: Option<&str>) -> WorkerManifest {
    let mut manifest = base_manifest(temp, "capability");
    manifest.provider_name = "custom:https://model-chosen.example/v1".to_string();
    manifest.api_key = api_key.map(str::to_string);
    manifest
}

/// C0-C / R-C0 F-6, consumer 1: the worker's own chain on a provider the
/// parent took from a model's `provider` argument gets neither the root
/// connection nor the root rotation keys.
#[test]
fn a_non_default_worker_chain_gets_no_root_connection_or_rotation_keys() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let config = root_config(temp.path());
    let manifest = non_default_manifest(temp.path(), None);

    let credentials = worker_credentials(&manifest, &config);
    assert_eq!(credentials.provider.api_key, None);
    assert_eq!(credentials.provider.api_url, None);
    assert!(
        credentials.reliability.api_keys.is_empty(),
        "rotation keys belong to the default provider"
    );
}

/// Consumer 2, the process-mode two-hop: the worker's `sessions_spawn`
/// pairs its fallback with the worker's own provider, so the fallback is
/// that provider's own credential and never the root key.
#[test]
fn a_non_default_worker_spawn_fallback_is_never_the_root_key() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let config = root_config(temp.path());

    let keyless = non_default_manifest(temp.path(), None);
    assert_eq!(
        worker_credentials(&keyless, &config).spawn_fallback,
        None,
        "the root key must not become the fallback for the worker's own provider"
    );

    let keyed = non_default_manifest(temp.path(), Some(WORKER_AGENT_SENTINEL));
    assert_eq!(
        worker_credentials(&keyed, &config).spawn_fallback,
        Some(WORKER_AGENT_SENTINEL)
    );
}

/// Consumer 3, the root slot: the base tools' `delegate` pairs this slot
/// with the default provider, so it is the root key and never an agent key.
#[test]
fn a_worker_root_slot_is_never_an_agent_key() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let config = root_config(temp.path());
    let manifest = non_default_manifest(temp.path(), Some(WORKER_AGENT_SENTINEL));

    assert_eq!(
        worker_credentials(&manifest, &config).root_api_key,
        Some(WORKER_ROOT_SENTINEL),
        "an agent key must not stand in for the root key"
    );
}

async fn admitted_worker_fallback_spawn() {
    use crate::providers::credential_probe::CredentialProbe;
    let temp = tempfile::tempdir().expect("test: temp dir");
    let own = CredentialProbe::start().await;
    let fallback = CredentialProbe::start().await;
    let mut config = root_config(temp.path());
    config.reliability.fallback_providers = vec![fallback.custom_provider()];
    let mut manifest = base_manifest(temp.path(), "capability");
    manifest.provider_name = own.custom_provider();
    manifest.api_key = None;
    manifest.workspace_dir = temp.path().to_path_buf();
    config.causal_tree.enabled = false;
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider": manifest.provider_name,
            "model": manifest.model,
            "operations": ["chat"],
            "native_tools": true
        }))
        .expect("test: worker model metadata"),
    );
    config.cost.prices.insert(
        format!("{}/{}", manifest.provider_name, manifest.model),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.cost.prices.insert(
        format!("{}/{}", fallback.custom_provider(), manifest.model),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let credentials = worker_credentials(&manifest, &config);
    let provider: Arc<dyn Provider> =
        Arc::from(worker_provider(&manifest, &config, &credentials, &options).expect("test: keyless endpoint builds"));
    let security = Arc::new(SecurityPolicy {
        workspace_dir: temp.path().to_path_buf(),
        ..SecurityPolicy::default()
    });
    let memory: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: worker spawn memory"));
    let source_fabric = MemoryFabric::new(Arc::clone(&memory), temp.path().to_string_lossy().into_owned())
        .with_event_recording(config.memory.event_recording_config());
    let active_runs = Arc::new(tokio::sync::RwLock::new(Vec::new()));
    let tool = worker_spawn_tool(
        &manifest,
        &config,
        &credentials,
        security,
        &options,
        Arc::clone(&active_runs),
    )
    .with_shared_memory(Arc::clone(&memory))
    .with_source_fabric(source_fabric);
    let tools_handle = tool.tools_handle();
    let registry = crate::agent::test_admission::registry_with_search(vec![Box::new(tool)]);
    assert!(tools_handle.set(&registry), "test: worker tool registry is set once");
    let mut admission_config = config.clone();
    admission_config.default_provider = Some(manifest.provider_name.clone());
    admission_config.default_model = Some(manifest.model.clone());
    let grant = crate::security::policy::ApprovalGrant::for_resource_operation(
        "sessions_spawn",
        "sessions_spawn:spawn",
        "test",
        None,
    );
    let message = ChannelMessage {
        id: "prx-worker-spawn-parent".to_owned(),
        sender: "prx_user".to_owned(),
        reply_target: "prx_worker".to_owned(),
        content: "sessions_spawn admitted worker fixture".to_owned(),
        channel: "session-worker".to_owned(),
        timestamp: 0,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        thread_ts: None,
        mentioned_uuids: Vec::new(),
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("test: worker spawn cleanup owner");
    let result = cleanup
        .scope_admission(async {
            let admitted = crate::agent::test_admission::AdmittedTurn::ordinary_controlled_channel_with_memory(
                admission_config,
                message,
                registry,
                memory,
                (&manifest.provider_name, &manifest.model),
            )
            .await;
            crate::runtime::child_source::scope_ordinary_provider_fixture(
                provider,
                manifest.provider_name.clone(),
                manifest.model.clone(),
                admitted.execute_tool(
                    "sessions_spawn",
                    serde_json::json!({
                        "task": "probe",
                        "provider": fallback.custom_provider(),
                        crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG: serde_json::to_value(grant)
                            .expect("test: grant serializes"),
                    }),
                ),
            )
            .await
        })
        .await
        .expect("test: admitted spawn returns a tool result");

    assert!(
        result.success,
        "an operator-configured provider is spawnable: {:?}",
        result.error
    );
    let request_count = fallback.wait_for_requests(1, std::time::Duration::from_secs(10)).await;
    assert_eq!(request_count, 1, "spawned worker state: {:?}", active_runs.read().await);
    assert!(!fallback.saw(WORKER_ROOT_SENTINEL) && !fallback.saw(WORKER_ROTATION_SENTINEL));
    cleanup
        .run(async { Ok(()) })
        .await
        .expect("test: worker spawn cleanup is confirmed");
}

/// R-C0 F-5: a worker's `sessions_spawn` accepts every provider the
/// operator configured, here a keyless fallback provider, like every other
/// surface; it previously refused anything but its own and its agents'.
#[tokio::test]
async fn a_worker_spawn_accepts_an_operator_configured_fallback_provider() {
    boxed_worker_segments(admitted_worker_fallback_spawn).await;
}

/// PMM-C: a worker whose manifest names a model that cannot chat builds
/// no provider, and nothing is sent.
#[tokio::test]
async fn a_worker_for_a_decision_only_model_builds_no_provider() {
    use crate::providers::credential_probe::CredentialProbe;
    let temp = tempfile::tempdir().expect("test: temp dir");
    let probe = CredentialProbe::start().await;
    let mut config = root_config(temp.path());
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"{}\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
            probe.custom_provider()
        ))
        .expect("test: declaration"),
    );
    let mut manifest = base_manifest(temp.path(), "capability");
    manifest.provider_name = probe.custom_provider();
    manifest.model = "jev-decision-test".to_string();
    manifest.api_key = None;
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let credentials = worker_credentials(&manifest, &config);
    let Err(error) = worker_provider(&manifest, &config, &credentials, &options) else {
        panic!("a worker for a decision-only model built its provider");
    };
    assert!(format!("{error:#}").contains("cannot be used for chat"), "{error:#}");
    assert!(probe.requests().is_empty());
}

/// Finding 64: exercise the worker's own credential-choice and provider
/// construction seam, rather than calling its lower-level typed builder.
#[tokio::test]
async fn worker_provider_refuses_a_cross_dialect_fallback_before_transport() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let probe = crate::providers::credential_probe::CredentialProbe::start().await;
    let mut config = root_config(temp.path());
    crate::providers::entrypoint_consistency_tests::configure_cross_dialect(&mut config, &probe.custom_provider());
    let mut manifest = base_manifest(temp.path(), "capability");
    manifest.provider_name = probe.custom_provider();
    manifest.model = crate::providers::entrypoint_consistency_tests::PRIMARY_MODEL.to_string();
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let credentials = worker_credentials(&manifest, &config);

    let error = worker_provider(&manifest, &config, &credentials, &options)
        .err()
        .expect("a worker accepted an incompatible fallback");
    let shown = format!("{error:#}");
    crate::providers::entrypoint_consistency_tests::assert_cross_dialect_refusal("worker", &shown);
    assert!(probe.requests().is_empty(), "the worker reached transport: {shown}");
}

/// The worker's own chain on a non-default provider carries neither the
/// root key nor a rotation key onto the wire.
#[tokio::test]
async fn a_worker_provider_on_a_non_default_endpoint_carries_no_root_credential() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
    let temp = tempfile::tempdir().expect("test: temp dir");
    let probe = CredentialProbe::start_with_statuses(vec![429]).await;
    let config = root_config(temp.path());
    let mut manifest = base_manifest(temp.path(), "capability");
    manifest.provider_name = probe.custom_provider();
    manifest.model = PROBE_MODEL.to_string();
    manifest.api_key = None;
    let options = crate::providers::provider_runtime_options_from_config(&config);

    let credentials = worker_credentials(&manifest, &config);
    let provider = worker_provider(&manifest, &config, &credentials, &options).expect("test: keyless endpoint builds");
    let _reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

    assert!(!probe.requests().is_empty());
    assert!(!probe.saw(WORKER_ROOT_SENTINEL));
    assert!(
        !probe.saw(WORKER_ROTATION_SENTINEL),
        "a rotation key reached a worker's non-default provider"
    );
}

/// C0-f F-1: the root URL and rotation keys belong to the root key. A
/// manifest key that is a named agent's own credential for the default
/// provider identity is presented without them.
#[test]
fn an_agent_key_on_the_default_provider_does_not_borrow_the_root_url() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let config = root_config(temp.path());
    let mut manifest = base_manifest(temp.path(), "capability");
    manifest.provider_name = "OPENAI".to_string();
    manifest.api_key = Some(WORKER_AGENT_SENTINEL.to_string());

    let credentials = worker_credentials(&manifest, &config);
    assert_eq!(credentials.provider.api_key, Some(WORKER_AGENT_SENTINEL));
    assert_eq!(
        credentials.provider.api_url, None,
        "the root URL belongs to the root key"
    );
    assert!(
        credentials.reliability.api_keys.is_empty(),
        "rotation keys belong to the root key"
    );

    manifest.api_key = Some(WORKER_ROOT_SENTINEL.to_string());
    manifest.direct_uses_root_connection = true;
    let credentials = worker_credentials(&manifest, &config);
    assert_eq!(credentials.provider.api_key, Some(WORKER_ROOT_SENTINEL));
    assert_eq!(credentials.provider.api_url, Some("https://api.openai.example/v1"));
}

/// Guard: a worker on the configured default keeps the root connection and
/// the rotation keys, in any spelling of that provider.
#[test]
fn a_worker_on_the_default_provider_keeps_the_root_connection() {
    let temp = tempfile::tempdir().expect("test: temp dir");
    let config = root_config(temp.path());
    let mut manifest = base_manifest(temp.path(), "capability");
    manifest.provider_name = "OpenAI".to_string();
    manifest.api_key = Some(WORKER_ROOT_SENTINEL.to_string());
    manifest.direct_uses_root_connection = true;
    validate_worker_route(&manifest, &config).expect("root manifest matches config");

    let credentials = worker_credentials(&manifest, &config);
    assert_eq!(credentials.provider.api_key, Some(WORKER_ROOT_SENTINEL));
    assert_eq!(credentials.provider.api_url, Some("https://api.openai.example/v1"));
    assert_eq!(
        credentials.reliability.api_keys,
        vec![WORKER_ROTATION_SENTINEL.to_string()]
    );
    assert_eq!(credentials.spawn_fallback, Some(WORKER_ROOT_SENTINEL));
}

#[test]
fn session_worker_secret_follows_the_resolved_config_dir_instead_of_home() {
    let _lock = crate::config::schema::process_config_dir_test_lock();
    let scoped = std::path::PathBuf::from("/tmp/openprx-secret-scope");
    let previous = crate::config::schema::swap_process_config_dir_for_tests(Some(scoped.clone()));
    let scoped_path = session_secret_path();
    crate::config::schema::swap_process_config_dir_for_tests(None);
    let unscoped_path = session_secret_path();
    crate::config::schema::swap_process_config_dir_for_tests(previous);

    assert_eq!(
        scoped_path,
        Some(scoped.join("keys").join("session_worker.secret")),
        "a scoped run must keep its session-worker secret inside the resolved config dir"
    );
    assert_eq!(
        unscoped_path,
        crate::config::default_openprx_dir().map(|dir| dir.join("keys").join("session_worker.secret")),
        "without a published config dir the default ~/.openprx location is kept"
    );
}
