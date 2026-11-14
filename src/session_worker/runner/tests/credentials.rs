#![cfg(test)]

use super::base_manifest;
use crate::config::Config;
use crate::session_worker::protocol::WorkerManifest;
use crate::session_worker::runner::provider::worker_credentials;
use std::path::Path;

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

#[test]
fn session_worker_secret_follows_the_resolved_config_dir_instead_of_home() {
    let _lock = crate::config::schema::process_config_dir_test_lock();
    let scoped = std::path::PathBuf::from("/tmp/openprx-secret-scope");
    let previous = crate::config::schema::swap_process_config_dir_for_tests(Some(scoped.clone()));
    let scoped_path = crate::session_worker::transport_key::secret_path().ok();
    crate::config::schema::swap_process_config_dir_for_tests(None);
    let unscoped_path = crate::session_worker::transport_key::secret_path().ok();
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
