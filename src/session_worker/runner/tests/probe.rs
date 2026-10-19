#![cfg(test)]

use super::base_manifest;
use crate::memory::Memory;
use crate::session_worker::protocol::{WorkerManifest, WorkerResult};
use crate::session_worker::runner::run::run_validated_manifest;
use anyhow::Result;

pub(super) fn retain_worker_probe_directory(mut tmp: tempfile::TempDir) -> tempfile::TempDir {
    tmp.disable_cleanup(true);
    let subscriber = tracing_subscriber::fmt()
        .with_test_writer()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("worker probe retained owned scratch {}", tmp.path().display());
    });
    tmp
}

/// Drive the real process-worker entrypoint (`run_validated_manifest`)
/// against `provider_name`: a real parent config source, a shared-fabric
/// SQLite memory and the worker workspace all live under `tmp`.
async fn worker_probe_run(tmp: &tempfile::TempDir, provider_name: String) -> WorkerResult {
    with_worker_probe_manifest(tmp, provider_name, false, |manifest| async move {
        let config_dir_arg = manifest.config_dir.to_string_lossy().to_string();
        run_validated_manifest(manifest, None, Some(&config_dir_arg)).await
    })
    .await
    .expect("test: the worker runs to its result")
}

pub(super) async fn with_worker_probe_manifest<T, F, Fut>(
    tmp: &tempfile::TempDir,
    provider_name: String,
    generation_negative: bool,
    consume: F,
) -> Result<T>
where
    F: FnOnce(WorkerManifest) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    use crate::providers::credential_probe::PROBE_MODEL;
    // Retain only the existing driver's neutral decision request/reply evidence.
    // A fresh directory under the owned scratch cannot reuse an external target.
    let diagnostics = tmp.path().join("decision-evidence");
    let mut directory = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        directory.mode(0o700);
    }
    directory.create(&diagnostics)?;
    let metadata = std::fs::symlink_metadata(&diagnostics)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "owned decision evidence directory required"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o777 == 0o700,
            "private decision evidence directory required"
        );
    }
    let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
        .await?
        .retain_evidence_at(diagnostics);
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(!generation_negative, || async {
            let config_dir = tmp.path().join("config");
            let workspace = config_dir.join("workspace");
            std::fs::create_dir_all(&workspace)?;
            let mut config = crate::config::Config::default();
            config.config_path = config_dir.join("config.toml");
            config.workspace_dir = workspace.clone();
            config.default_provider = Some("ollama".into());
            config.default_model = Some("prx-local-model".into());
            config.memory.backend = "sqlite".into();
            config.autonomy.level = crate::security::AutonomyLevel::Full;
            config.sessions_spawn.default_mode = "process".into();
            config.sessions_spawn.cleanup_on_complete = false;
            config.providers.models.push(toml::from_str(
                "provider='ollama'\nmodel='prx-local-model'\noperations=['chat']\nnative_tools=true\n",
            )?);
            config.providers.models.push(serde_json::from_value(serde_json::json!({
                "provider":provider_name,"model":PROBE_MODEL,"operations":["chat"]
            }))?);
            config.cost.prices.insert(
                format!("{provider_name}/{PROBE_MODEL}"),
                crate::config::schema::ModelPricing {
                    input: 0.1,
                    output: 0.2,
                    cache_write: 0.3,
                    cache_read: 0.1,
                },
            );
            crate::agent::agent::native_decision_fixture::configure(&mut config, &endpoint)?;
            std::fs::write(&config.config_path, toml::to_string(&config)?)?;
            let mut manifest = base_manifest(&tmp.path().join("worker"), "unused-for-direct-validation");
            manifest.config_dir = config_dir;
            manifest.provider_name = provider_name;
            manifest.model = PROBE_MODEL.to_owned();
            manifest.task = "Describe the project.".to_owned();
            manifest.allowed_tools = Vec::new();
            manifest.identity_dir = None;
            manifest.memory_db_path = workspace.join("memory/brain.db");
            manifest.shared_memory_db_path = Some(manifest.memory_db_path.clone());
            manifest.memory_workspace_id = Some(workspace.to_string_lossy().into_owned());
            manifest.memory_event_recording = config.memory.event_recording_config();
            crate::runtime::turn_decision::with_worker_probe_manifest(config, manifest, consume).await
        })
        .await
}

/// Event types the worker's shared-fabric memory recorded for its workspace.
async fn worker_probe_event_types(tmp: &tempfile::TempDir) -> Vec<String> {
    let workspace = tmp.path().join("config/workspace");
    let memory = crate::memory::SqliteMemory::new(&workspace).expect("test: reopen worker memory");
    memory
        .list_message_events_recent(
            &crate::memory::MemoryPrincipal {
                require_owner_match: false,
                workspace_id: workspace.to_string_lossy().to_string(),
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            100,
        )
        .await
        .expect("test: worker events readable")
        .into_iter()
        .map(|event| event.event_type)
        .collect()
}

/// D-053 / REDACT-TURN probe: a process-mode worker whose provider reply
/// carries a secret runs `run_validated_manifest` through `finalize_turn`
/// (Completed) and the `worker.result.created` event; nothing under the temp
/// dir may keep the secret. It must fail before the REDACT-TURN commits.
#[tokio::test]
async fn session_worker_persists_no_secret_from_a_reply() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};
    use crate::providers::credential_probe::CredentialProbe;
    let tmp = tempfile::TempDir::new().expect("test: tempdir");
    let tmp = retain_worker_probe_directory(tmp);
    let probe = CredentialProbe::start_replying(&format!("Here is the key {PROBE_SECRET} you asked for")).await;

    let result = worker_probe_run(&tmp, probe.custom_provider()).await;

    assert!(result.success, "the worker must complete: {:?}", result.error);
    assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
    assert!(
        result.output.contains(PROBE_SECRET),
        "the reply must carry the secret for the probe to mean anything: {}",
        result.output
    );
    // Config source, worker workspace (cost ledger, audit log) and the
    // shared-fabric SQLite memory are all under this temp dir. Scan before
    // reopening the database so the scan sees exactly what the run left.
    assert_nothing_under_persists_the_probe_secret(tmp.path());
    let events = worker_probe_event_types(&tmp).await;
    assert!(
        events.iter().any(|event| event == "turn.finalized"),
        "the worker must reach the terminal commit: {events:?}"
    );
    assert!(
        events.iter().any(|event| event == "worker.result.created"),
        "the worker must record its result event: {events:?}"
    );
}

/// D-053 / REDACT-TURN probe: a process-mode worker whose provider fails
/// with an error carrying a secret runs `run_validated_manifest` through
/// `finalize_turn` (Failed) and the `worker.result.created` event; nothing
/// under the temp dir may keep the secret. It must fail before the
/// REDACT-TURN commits.
#[tokio::test]
async fn session_worker_persists_no_secret_from_a_provider_error() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};
    use crate::providers::credential_probe::CredentialProbe;
    let tmp = tempfile::TempDir::new().expect("test: tempdir");
    let tmp = retain_worker_probe_directory(tmp);
    let probe = CredentialProbe::start_failing(400, &format!("upstream rejected key {PROBE_SECRET}")).await;

    let result = worker_probe_run(&tmp, probe.custom_provider()).await;

    assert!(!result.success, "a failing upstream must fail the worker");
    assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
    let error = result.error.unwrap_or_default();
    // The upstream's error text must reach the turn: verbatim before the
    // redactors merged (D-054), already redacted by the provider layer since.
    assert!(
        error.contains(PROBE_SECRET) || error.contains("upstream rejected key [REDACTED]"),
        "the provider error must reach the turn for the probe to mean anything: {error}"
    );
    // Config source, worker workspace (cost ledger, audit log) and the
    // shared-fabric SQLite memory are all under this temp dir. Scan before
    // reopening the database so the scan sees exactly what the run left.
    assert_nothing_under_persists_the_probe_secret(tmp.path());
    let events = worker_probe_event_types(&tmp).await;
    assert!(
        events.iter().any(|event| event == "turn.finalized"),
        "the worker must reach the terminal commit: {events:?}"
    );
    assert!(
        events.iter().any(|event| event == "worker.result.created"),
        "the worker must record its result event: {events:?}"
    );
}
