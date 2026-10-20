#![cfg(test)]

use super::base_manifest;
use super::probe::{retain_worker_probe_directory, with_worker_probe_manifest};
use crate::session_worker::protocol::WorkerManifest;
use crate::session_worker::runner::capability::{
    compute_worker_capability, manifest_signing_payload, validate_worker_capability_with_env,
};
use crate::session_worker::runner::paths::{normalized_worker_memory_strategy, parse_tools_override};
use crate::session_worker::runner::run::run_validated_manifest;
use crate::session_worker::runner::validate::{
    validate_parent_memory_backend, validate_worker_cli_overrides, validate_worker_manifest_with_capability_env,
};

#[test]
fn parse_tools_override_accepts_string_array() {
    let parsed = parse_tools_override(r#"["shell","file_read"]"#).unwrap();
    assert_eq!(parsed, vec!["shell".to_string(), "file_read".to_string()]);
}

#[test]
fn parse_tools_override_rejects_invalid_json_shape() {
    let error = parse_tools_override(r#"{"tool":"shell"}"#).unwrap_err();
    assert!(error.to_string().contains("parse --tools JSON as string array"));
}

#[test]
fn worker_manifest_rejects_hybrid_memory_without_merge_consumer() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let mut manifest = base_manifest(tmp.path(), "capability-a");

    assert_eq!(normalized_worker_memory_strategy(&manifest).unwrap(), "shared_fabric");
    manifest.memory_strategy = Some("isolated_private".to_string());
    assert_eq!(
        normalized_worker_memory_strategy(&manifest).unwrap(),
        "isolated_private"
    );
    manifest.memory_strategy = Some("hybrid".to_string());
    manifest.memory_db_path = manifest.worker_memory_db_path.clone().unwrap();

    let serialized = serde_json::to_string(&manifest).unwrap();
    let parsed: WorkerManifest = serde_json::from_str(&serialized).unwrap();
    assert_eq!(parsed.memory_strategy.as_deref(), Some("hybrid"));

    let expiry = cap_now() + 300;
    let (manifest, cap, expiry) = seal(parsed, expiry);
    set_expiry_env(expiry);
    let error = validate_worker_manifest_with_capability_env(&manifest, Some(&cap))
        .unwrap_err()
        .to_string();
    clear_expiry_env();
    assert_eq!(error, crate::config::HYBRID_PROCESS_MEMORY_UNAVAILABLE);
}

#[test]
fn worker_manifest_rejects_parent_backend_drift() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut manifest = base_manifest(tmp.path(), "capability-a");
    manifest.memory_backend = "postgres".to_string();

    let error = validate_parent_memory_backend(&manifest, "sqlite").unwrap_err();
    assert!(error.to_string().contains("manifest=postgres, config=sqlite"));
    validate_parent_memory_backend(&manifest, "postgres").unwrap();
}

#[test]
fn shared_worker_uses_configured_parent_memory_factory() {
    let source = include_str!("../run.rs");
    let shared_branch = source
        .split_once(
            "let memory: Arc<dyn Memory> = if normalized_worker_memory_strategy(&manifest)? == \"shared_fabric\"",
        )
        .unwrap()
        .1
        .split_once("} else {")
        .unwrap()
        .0;
    assert!(shared_branch.contains("create_memory_with_storage_and_routes_with_acl"));
    assert!(shared_branch.contains("Some(&config.storage.provider.config)"));
    assert!(!shared_branch.contains("SqliteMemory"));
}

#[test]
fn worker_manifest_validation_requires_parent_capability() {
    let tmp = tempfile::TempDir::new().unwrap();
    let manifest = base_manifest(tmp.path(), "capability-a");

    let error = validate_worker_manifest_with_capability_env(&manifest, None).unwrap_err();
    assert!(error.to_string().contains("parent capability env is missing"));
}

/// Serialize tests that mutate the process-global capability expiry env.
static CAP_ENV_GUARD: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn cap_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seal `manifest` with a valid HMAC for `expiry`, embedding the token into
/// the manifest and returning `(sealed_manifest, capability, expiry)`.
fn seal(mut manifest: WorkerManifest, expiry: u64) -> (WorkerManifest, String, u64) {
    manifest.parent_capability = None;
    let payload = manifest_signing_payload(&manifest).expect("payload");
    let cap = compute_worker_capability(&manifest.run_id, expiry, &payload);
    manifest.parent_capability = Some(cap.clone());
    (manifest, cap, expiry)
}

fn set_expiry_env(expiry: u64) {
    // SAFETY: tests hold `CAP_ENV_GUARD`, serializing all env mutation.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY", expiry.to_string());
    }
}

fn clear_expiry_env() {
    // SAFETY: tests hold `CAP_ENV_GUARD`, serializing all env mutation.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY");
    }
}

#[test]
fn worker_manifest_validation_rejects_path_escape() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let mut manifest = base_manifest(tmp.path(), "");
    manifest.identity_dir = Some("../outside".to_string());
    let expiry = cap_now() + 300;
    let (manifest, cap, expiry) = seal(manifest, expiry);
    set_expiry_env(expiry);

    let error = validate_worker_manifest_with_capability_env(&manifest, Some(&cap)).unwrap_err();
    clear_expiry_env();
    assert!(error.to_string().contains("identity_dir"));
}

#[test]
fn valid_sealed_capability_accepted() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let expiry = cap_now() + 300;
    let (manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
    set_expiry_env(expiry);
    let result = validate_worker_capability_with_env(&manifest, Some(&cap));
    clear_expiry_env();
    result.expect("valid sealed capability must be accepted");
}

#[test]
fn legacy_uuid_capability_rejected() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let expiry = cap_now() + 300;
    let (mut manifest, _cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
    // Replace the sealed token with a legacy UUID in both manifest and env.
    let uuid = "550e8400-e29b-41d4-a716-446655440000".to_string();
    manifest.parent_capability = Some(uuid.clone());
    set_expiry_env(expiry);
    let error = validate_worker_capability_with_env(&manifest, Some(&uuid)).unwrap_err();
    clear_expiry_env();
    assert!(error.to_string().contains("signature mismatch"));
}

#[test]
fn tampered_manifest_capability_rejected() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let expiry = cap_now() + 300;
    let (mut manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
    // Escalate the allow-list after sealing; the embedded token now mismatches.
    manifest.allowed_tools.push("shell".to_string());
    set_expiry_env(expiry);
    let error = validate_worker_capability_with_env(&manifest, Some(&cap)).unwrap_err();
    clear_expiry_env();
    assert!(error.to_string().contains("signature mismatch"));
}

#[test]
fn tampered_route_provenance_breaks_worker_capability() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _guard = CAP_ENV_GUARD.lock();
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let mut original = base_manifest(temp.path(), "");
    original.route_hint = Some("fast".to_string());
    original.route_source_provider = Some("openai".to_string());
    original.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
        crate::providers::credentials::LegacySlot::ModelRoute("fast".to_string()),
    ));
    let expiry = cap_now() + 300;
    let (sealed, capability, expiry) = seal(original, expiry);
    set_expiry_env(expiry);
    let mut source_changed = sealed.clone();
    source_changed.route_source_provider = Some("anthropic".to_string());
    assert!(
        validate_worker_capability_with_env(&source_changed, Some(&capability))
            .expect_err("source tamper refused")
            .to_string()
            .contains("signature mismatch")
    );
    let mut credential_changed = sealed;
    credential_changed.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
        crate::providers::credentials::LegacySlot::Root,
    ));
    assert!(
        validate_worker_capability_with_env(&credential_changed, Some(&capability))
            .expect_err("credential tamper refused")
            .to_string()
            .contains("signature mismatch")
    );
    clear_expiry_env();
}

#[test]
fn tampered_config_generation_capability_rejected() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let expiry = cap_now() + 300;
    let (mut manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
    manifest.config_generation = "f".repeat(64);
    set_expiry_env(expiry);
    let error = validate_worker_capability_with_env(&manifest, Some(&cap)).unwrap_err();
    clear_expiry_env();
    assert!(error.to_string().contains("signature mismatch"));
}

#[test]
fn wrong_run_id_capability_rejected() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let expiry = cap_now() + 300;
    let (mut manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
    // Replay the token under a different run id.
    manifest.run_id = "different-run".to_string();
    set_expiry_env(expiry);
    let error = validate_worker_capability_with_env(&manifest, Some(&cap)).unwrap_err();
    clear_expiry_env();
    assert!(error.to_string().contains("signature mismatch"));
}

#[test]
fn expired_capability_rejected() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let stale = cap_now().saturating_sub(10);
    let (manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), stale);
    set_expiry_env(expiry);
    let error = validate_worker_capability_with_env(&manifest, Some(&cap)).unwrap_err();
    clear_expiry_env();
    assert!(error.to_string().contains("expired"));
}

#[test]
fn env_manifest_capability_desync_rejected() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let expiry = cap_now() + 300;
    let (manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
    set_expiry_env(expiry);
    // Env token differs from the manifest's sealed token.
    let mut wrong = cap;
    wrong.push('x');
    let error = validate_worker_capability_with_env(&manifest, Some(&wrong)).unwrap_err();
    clear_expiry_env();
    assert!(error.to_string().contains("mismatch"));
}

#[test]
fn worker_cli_overrides_must_match_manifest() {
    let tmp = tempfile::TempDir::new().unwrap();
    let manifest = base_manifest(tmp.path(), "capability-a");

    let error = validate_worker_cli_overrides(
        &manifest,
        Some("different task"),
        Some(&manifest.workspace_dir.to_string_lossy()),
        Some(&manifest.memory_db_path.to_string_lossy()),
        Some(&manifest.allowed_tools),
        Some(&manifest.config_dir.to_string_lossy()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("task override"));
}

#[test]
fn worker_cli_config_dir_must_match_sealed_manifest() {
    let tmp = tempfile::TempDir::new().unwrap();
    let manifest = base_manifest(tmp.path(), "capability-a");

    let error = validate_worker_cli_overrides(
        &manifest,
        Some(&manifest.task),
        Some(&manifest.workspace_dir.to_string_lossy()),
        Some(&manifest.memory_db_path.to_string_lossy()),
        Some(&manifest.allowed_tools),
        Some("/different/config"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("config-dir"));
}

// The `CAP_ENV_GUARD` mutex intentionally stays locked across the awaited
// validate-then-run sequence so the capability env stays stable
// for the whole run; this single-threaded test never contends the guard.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn malicious_manifest_rejected_before_config_memory_or_worker_dir_creation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let workspace = tmp.path().join("worker");
    let memory_db = workspace.join("memory").join("brain.db");
    let config_dir_arg = config_dir.to_string_lossy().to_string();

    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _g = CAP_ENV_GUARD.lock();
    let mut manifest = base_manifest(&workspace, "");
    manifest.run_id = "../escape".to_string();
    // Seal with a valid HMAC so validation passes the capability check and
    // reaches the run_id path-safety check we are asserting on.
    let expiry = cap_now() + 300;
    let (manifest, cap, expiry) = seal(manifest, expiry);
    set_expiry_env(expiry);

    // `run_manifest`'s order: validate, and only a valid manifest runs
    let error = async {
        validate_worker_manifest_with_capability_env(&manifest, Some(&cap))?;
        run_validated_manifest(manifest, None, Some(&config_dir_arg)).await
    }
    .await
    .unwrap_err();
    clear_expiry_env();
    assert!(error.to_string().contains("run_id"));
    assert!(!config_dir.exists(), "invalid manifest must not initialize config dir");
    assert!(!workspace.exists(), "invalid manifest must not create worker workspace");
    assert!(
        !memory_db.exists(),
        "invalid manifest must not initialize worker memory DB"
    );
}

#[tokio::test]
async fn worker_missing_config_source_fails_without_initializing_defaults() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("missing-config");
    let workspace = tmp.path().join("worker");
    let config_dir_arg = config_dir.to_string_lossy().to_string();
    let mut manifest = base_manifest(&workspace, "unused-for-direct-validation");
    manifest.config_dir = config_dir.clone();

    let result = run_validated_manifest(manifest, None, Some(&config_dir_arg)).await;

    assert!(result.is_err());
    assert!(
        !config_dir.exists(),
        "session-worker must not initialize a missing parent config source"
    );
}

#[tokio::test]
async fn worker_rejects_changed_config_generation_before_workspace_side_effects() {
    let tmp = tempfile::TempDir::new().unwrap();
    let tmp = retain_worker_probe_directory(tmp);
    let config_dir = tmp.path().join("config");
    let workspace = tmp.path().join("worker");
    let error = with_worker_probe_manifest(
        &tmp,
        "custom:http://127.0.0.1:1".to_owned(),
        true,
        |manifest| async move {
            // The real parent has already issued this manifest against the
            // actual config bytes. Change only those private bytes afterwards.
            let config_path = config_dir.join("config.toml");
            let mut config: crate::config::Config = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
            config.default_temperature = 0.6;
            std::fs::write(config_path, toml::to_string(&config)?)?;
            let config_dir_arg = config_dir.to_string_lossy().to_string();
            run_validated_manifest(manifest, None, Some(&config_dir_arg)).await
        },
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("generation mismatch"));
    assert!(
        !workspace.exists(),
        "generation mismatch must fail before worker side effects"
    );
}

#[test]
fn temperature_override_is_signed_and_absent_from_legacy_payloads() {
    let _config_dir = crate::config::schema::process_config_dir_test_lock();
    let _guard = CAP_ENV_GUARD.lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let mut manifest = base_manifest(tmp.path(), "");
    let value = serde_json::to_value(&manifest).unwrap();
    assert!(value.get("temperature_override").is_none());
    manifest.temperature_override = Some(manifest.temperature);
    let (mut manifest, capability, expiry) = seal(manifest, cap_now() + 300);
    set_expiry_env(expiry);
    let valid = validate_worker_capability_with_env(&manifest, Some(&capability));
    manifest.temperature_override = Some(0.2);
    let tampered = validate_worker_capability_with_env(&manifest, Some(&capability));
    clear_expiry_env();
    valid.expect("test: signed caller temperature");
    assert!(tampered.unwrap_err().to_string().contains("signature mismatch"));
}

#[tokio::test]
async fn signed_temperature_override_must_match_the_sealed_request_before_resources_open() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut manifest = base_manifest(tmp.path(), "");
    manifest.temperature_override = Some(0.2);
    let error = run_validated_manifest(manifest, None, None).await.unwrap_err();
    assert!(format!("{error:#}").contains("worker caller temperature differs from its sealed temperature"));
    assert!(!tmp.path().join("config.toml").exists());
}
