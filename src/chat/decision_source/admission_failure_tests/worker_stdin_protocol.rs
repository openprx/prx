#![cfg(test)]
#![cfg(unix)]
//! Original stdin probes through genuine local /bg process issuance.
use super::*;
use crate::session_worker::protocol::WorkerResult;
use crate::tools::Tool;
use crate::tools::sessions_spawn::{
    ProcessFinalization, ProcessTerminationRequestResult, SessionsSpawnTool, SubAgentRun, TestWorkerProtocolCase,
    TestWorkerProtocolControl,
};
use futures_util::FutureExt as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const POST_AUTH_FAILURE: &str = "session-worker config source is missing";
const WORKER_WATCHDOG: Duration = Duration::from_mins(3);

struct WorkerRun {
    status: std::process::ExitStatus,
    stdout: String,
}

struct ParentConfigScope {
    previous: Option<PathBuf>,
    _lock: parking_lot::MutexGuard<'static, ()>,
}
impl Drop for ParentConfigScope {
    fn drop(&mut self) {
        crate::config::schema::swap_process_config_dir_for_tests(self.previous.take());
    }
}

struct NoAnnouncements;
#[async_trait::async_trait]
impl crate::channels::traits::Channel for NoAnnouncements {
    fn name(&self) -> &str {
        "terminal"
    }
    async fn send(&self, _: &crate::channels::traits::SendMessage) -> anyhow::Result<()> {
        Ok(())
    }
    async fn listen(
        &self,
        _: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("worker protocol fixture must not start a listener")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactFile {
    path: PathBuf,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerArtifact {
    schema: String,
    profile: String,
    binary: ArtifactFile,
    source_manifest: ArtifactFile,
    build_evidence: ArtifactFile,
}

fn checked_file(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(path.is_absolute(), "artifact path must be absolute");
    let mut prefix = PathBuf::new();
    for component in path.components() {
        anyhow::ensure!(
            matches!(component, Component::RootDir | Component::Normal(_)),
            "artifact path must not contain traversal"
        );
        prefix.push(component.as_os_str());
        let kind = std::fs::symlink_metadata(&prefix)?.file_type();
        anyhow::ensure!(
            if prefix == path { kind.is_file() } else { kind.is_dir() },
            "artifact path contains a symlink or special node"
        );
    }
    Ok(())
}

fn checked_hash(artifact: &ArtifactFile) -> anyhow::Result<()> {
    use std::io::Read as _;
    checked_file(&artifact.path)?;
    anyhow::ensure!(
        artifact.sha256.len() == 64
            && artifact
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "artifact digest must be lowercase SHA-256"
    );
    let mut input = std::fs::File::open(&artifact.path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(buffer.get(..count).expect("read count must fit the supplied buffer"));
    }
    anyhow::ensure!(
        hex::encode(hash.finalize()) == artifact.sha256,
        "artifact digest mismatch"
    );
    Ok(())
}

fn worker_artifact() -> anyhow::Result<WorkerArtifact> {
    // Mandatory external bindings are supplied by the reviewed guarded command.
    // A self-authored JSON claim is never sufficient to select an executable.
    let parameter = ArtifactFile {
        path: PathBuf::from(std::env::var("PRX_C28_ARTIFACT_MANIFEST")?),
        sha256: std::env::var("PRX_C28_ARTIFACT_MANIFEST_SHA256")?,
    };
    checked_hash(&parameter)?;
    anyhow::ensure!(
        std::fs::metadata(&parameter.path)?.len() <= 16 * 1024,
        "artifact parameter manifest too large"
    );
    let artifact: WorkerArtifact = serde_json::from_slice(&std::fs::read(&parameter.path)?)?;
    anyhow::ensure!(
        artifact.schema == "prx-c28-prebuilt-v1",
        "unexpected worker artifact contract"
    );
    anyhow::ensure!(
        artifact.profile == std::env::var("PRX_C28_EXPECTED_PROFILE")?
            && matches!(artifact.profile.as_str(), "dev" | "release")
            && artifact.source_manifest.sha256 == std::env::var("PRX_C28_EXPECTED_SOURCE_SHA256")?,
        "worker source/profile differs from the independently frozen expected build"
    );
    checked_hash(&artifact.binary)?;
    checked_hash(&artifact.source_manifest)?;
    checked_hash(&artifact.build_evidence)?;
    Ok(artifact)
}

fn assert_single_json_line(stdout: &str) -> WorkerResult {
    assert!(
        stdout.ends_with('\n'),
        "session-worker stdout must be newline-terminated, got {stdout:?}"
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        1,
        "session-worker stdout must be exactly one JSON line, got {stdout:?}"
    );
    serde_json::from_str::<WorkerResult>(lines.first().copied().unwrap_or_default())
        .expect("session-worker stdout line parses as a WorkerResult")
}

async fn wait_owned_run(
    runs: &Arc<tokio::sync::RwLock<Vec<SubAgentRun>>>,
    protocol: &TestWorkerProtocolControl,
) -> anyhow::Result<()> {
    tokio::time::timeout(WORKER_WATCHDOG, async {
        loop {
            anyhow::ensure!(
                !protocol.serialized_precondition_failed(),
                "worker stdin fixture serialized precondition failed before spawn"
            );
            {
                let runs = runs.read().await;
                anyhow::ensure!(runs.len() == 1, "exactly one actual operator run required");
                let run = runs.first().context("actual operator run missing")?;
                let control = run.process_control.as_ref().context("actual process owner missing")?;
                if let Some(finalization) = control.finalization() {
                    anyhow::ensure!(
                        finalization == ProcessFinalization::Natural,
                        "worker did not finish naturally"
                    );
                    if run.finished_at.is_some() {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("actual worker completion watchdog")?
}

async fn cleanup_owned_runs(runs: &Arc<tokio::sync::RwLock<Vec<SubAgentRun>>>) -> anyhow::Result<()> {
    let controls = {
        let runs = runs.read().await;
        runs.iter()
            .map(|run| run.process_control.clone().context("fixture run lacks process owner"))
            .collect::<anyhow::Result<Vec<_>>>()?
    };
    for control in controls {
        match control.finalization() {
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated) => {}
            Some(_) => anyhow::bail!("worker cleanup remains unconfirmed"),
            None => {
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    control.request_termination("worker protocol fixture cleanup"),
                )
                .await
                .context("worker cleanup watchdog")?;
                anyhow::ensure!(
                    matches!(
                        result,
                        ProcessTerminationRequestResult::Finalized(
                            ProcessFinalization::Natural | ProcessFinalization::Terminated
                        )
                    ),
                    "worker cleanup remains unconfirmed"
                );
            }
        }
    }
    Ok(())
}

async fn run_protocol_case(case: TestWorkerProtocolCase) -> anyhow::Result<WorkerRun> {
    let artifact = worker_artifact()?;
    let scratch = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let private_home = scratch.path().join("worker-home");
    let config_dir = private_home.join("config");
    let workspace = config_dir.join("workspace");
    let workers = private_home.join("workers");
    anyhow::ensure!(
        std::fs::symlink_metadata(scratch.path())?.permissions().mode() & 0o777 == 0o700,
        "owned scratch directory must be private at creation"
    );
    for path in [
        private_home.clone(),
        config_dir.clone(),
        workspace.clone(),
        workers.clone(),
        workspace.join("memory"),
        workspace.join("self"),
        workspace.join("self").join("causal_tree"),
        workspace.join("state"),
    ] {
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        anyhow::ensure!(
            std::fs::symlink_metadata(&path)?.permissions().mode() & 0o777 == 0o700,
            "owned fixture directory must be private at creation"
        );
    }
    let config_lock = crate::config::schema::process_config_dir_test_lock();
    let _config_scope = ParentConfigScope {
        previous: crate::config::schema::swap_process_config_dir_for_tests(Some(config_dir.clone())),
        _lock: config_lock,
    };
    let runs = Arc::new(tokio::sync::RwLock::new(Vec::<SubAgentRun>::new()));
    let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let cleanup_scope = crate::runtime::turn_cleanup::CleanupScope::new(Some(&owner), CancellationToken::new())?;
    let control = Arc::new(TestWorkerProtocolControl::new(case));
    let scenario = std::panic::AssertUnwindSafe(cleanup_scope.run(Box::pin(async {
        let mut config = crate::config::Config::default();
        config.config_path = config_dir.join("config.toml");
        config.workspace_dir = workspace.clone();
        config.default_provider = Some("ollama".to_owned());
        config.default_model = Some("prx-local-model".to_owned());
        config.memory.backend = "sqlite".to_owned();
        config.causal_tree.enabled = true;
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        config.sessions_spawn.default_mode = "process".to_owned();
        config.sessions_spawn.process_memory_strategy = "isolated_private".to_owned();
        config.sessions_spawn.worker_workspace_root = Some(workers.to_string_lossy().into_owned());
        config.sessions_spawn.cleanup_on_complete = false;
        config.providers.models.push(toml::from_str(
            "provider='ollama'\nmodel='prx-local-model'\noperations=['chat']\nnative_tools=true\n",
        )?);
        let mut config_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config.config_path)?;
        config_file.write_all(toml::to_string(&config)?.as_bytes())?;
        config_file.sync_all()?;
        anyhow::ensure!(
            config_file.metadata()?.permissions().mode() & 0o777 == 0o600,
            "fixture config must be private at creation"
        );
        drop(config_file);
        let generation = crate::config::new_shared(config.clone()).pin();
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::SqliteMemory::new(&workspace)?);
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy())
            .with_event_recording(config.memory.event_recording_config());
        let security = Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &workspace,
        ));
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::ollama::OllamaProvider::new(None, None));
        let binding = ChatProviderBinding::capture(&generation, &provider, "ollama", "prx-local-model")?
            .context("actual operator provider binding missing")?;
        let spawn = SessionsSpawnTool::new_with_registry(
            Arc::new(NoAnnouncements),
            Arc::clone(&provider),
            "ollama",
            "prx-local-model",
            0.0,
            Arc::clone(&security),
            workspace.clone(),
            config.multimodal.clone(),
            config.agent.compaction.clone(),
            Default::default(),
            None,
            crate::providers::provider_runtime_options_from_config(&config),
            config.sessions_spawn.clone(),
            Arc::clone(&runs),
        )
        .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(&config))
        .with_hint_config(&config)
        .with_model_catalog(crate::providers::selection::catalog_for(&config))
        .with_shared_memory(Arc::clone(&memory))
        .with_source_fabric(fabric)
        .with_event_recording(config.memory.event_recording_config())
        .with_test_worker_executable(artifact.binary.path, artifact.binary.sha256, private_home.clone())?
        .with_test_worker_protocol(Arc::clone(&control))?;
        let tools_handle = spawn.tools_handle();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(spawn)]);
        assert!(tools_handle.set(&registry));
        let publication = crate::runtime::turn_publication::TurnDecisionPublication {
            generation: Arc::clone(&generation),
            registry: Arc::clone(&registry),
            mcp_tool: None,
            security: Arc::clone(&security),
            observer: Arc::new(crate::observability::noop::NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(workspace.clone())),
            memory: Arc::clone(&memory),
            plugin_generation: 0,
        }
        .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
        .await?;
        owner.retain_service(Arc::clone(&publication));
        let task = "noop";
        let message = crate::channels::traits::ChannelMessage {
            id: "prx-worker-protocol-command".to_owned(),
            sender: "user".to_owned(),
            channel: "terminal".to_owned(),
            content: format!("/bg {task}"),
            ..Default::default()
        };
        let command = ChatOperatorSpawn::capture(
            &message,
            task,
            &binding,
            &generation,
            &provider,
            "ollama",
            "prx-local-model",
        )?;
        let (args, source) = command
            .admit(
                Arc::clone(&memory),
                "prx_worker_protocol_session",
                Arc::clone(&security),
                publication,
            )
            .await?;
        source
            .scope(async {
                let tool = registry
                    .iter()
                    .find(|tool| tool.name() == "sessions_spawn")
                    .context("published spawn tool missing")?;
                let result = tool.execute(args).await?;
                anyhow::ensure!(result.success, "actual operator process launch rejected");
                wait_owned_run(&runs, &control).await
            })
            .await?;
        let terminal = control.take_terminal()?;
        assert_eq!(
            control.termination_calls(),
            0,
            "natural exit must not request termination"
        );
        assert_eq!(
            terminal.stdin_held_through_reap,
            matches!(case, TestWorkerProtocolCase::HoldAfterThreeSteers)
        );
        anyhow::ensure!(
            terminal.stderr.len() <= 10 * 1024 * 1024 + 128,
            "worker stderr capture exceeds existing stream bound"
        );
        // Original fixture output cap, inspected only after actual process finalization.
        assert!(
            terminal.stdout.len() as u64 <= 1024 * 1024,
            "worker stdout exceeded fixture limit"
        );
        let stdout = String::from_utf8(terminal.stdout)?;
        Ok::<_, anyhow::Error>(WorkerRun {
            status: terminal.status,
            stdout,
        })
    })))
    .catch_unwind();
    let result = tokio::time::timeout(WORKER_WATCHDOG + Duration::from_secs(20), scenario).await;
    let cleanup_result = std::panic::AssertUnwindSafe(async {
        let process_cleanup = cleanup_owned_runs(&runs).await;
        let owner_cleanup = tokio::time::timeout(
            Duration::from_secs(5),
            owner.drain(tokio::time::Instant::now() + Duration::from_secs(5)),
        )
        .await;
        let process_class = match &process_cleanup {
            Ok(()) => "process_cleanup_returned_ok",
            Err(error) => {
                let text = error.to_string();
                if text.contains("watchdog") {
                    "process_cleanup_watchdog"
                } else if text.contains("remains unconfirmed") {
                    "process_cleanup_unconfirmed"
                } else if text.contains("lacks process owner") {
                    "process_owner_missing"
                } else {
                    "other_process_cleanup_error"
                }
            }
        };
        let owner_class = match &owner_cleanup {
            Ok(Ok(())) => "owner_drain_returned_ok",
            Ok(Err(_)) => "owner_drain_returned_error",
            Err(_) => "owner_drain_watchdog",
        };
        control.observe_cleanup_parts(process_class, owner_class);
        process_cleanup.is_ok() && matches!(owner_cleanup, Ok(Ok(())))
    })
    .catch_unwind()
    .await;
    let finalization = {
        let runs = runs.read().await;
        runs.first()
            .and_then(|run| run.process_control.as_ref())
            .and_then(|control| control.finalization())
    };
    control.observe_fixture_cleanup(
        finalization,
        match &cleanup_result {
            Ok(true) => "confirmed",
            Ok(false) => "unconfirmed",
            Err(_) => "cleanup_panicked",
        },
    );
    let diagnostic_retained = (|| -> anyhow::Result<()> {
        let bytes = control.diagnostic_json()?;
        anyhow::ensure!(bytes.len() <= 32 * 1024, "worker diagnostic byte bound");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(scratch.path().join("worker-protocol-diagnostic.json"))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    // Retain precondition failures as well as incomplete cleanup; do not rewrite either outcome.
    let keep_diagnostic = !matches!(&result, Ok(Ok(Ok(_)))) || diagnostic_retained.is_err();
    let confirmed = matches!(cleanup_result, Ok(true));
    if !confirmed || keep_diagnostic {
        let retained = scratch.keep();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = writeln!(
                std::io::stderr().lock(),
                "worker protocol retained diagnostic workspace: {}",
                retained.display()
            );
        }));
    }
    match result {
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Ok(Ok(result)) => {
            if control.serialized_precondition_failed() {
                // Preserve the real preflight failure; conservative settlement is in the private diagnostic.
                anyhow::bail!(
                    "worker stdin fixture serialized precondition failed before spawn; cleanup_confirmed={confirmed}"
                );
            }
            anyhow::ensure!(confirmed, "worker protocol cleanup unconfirmed");
            diagnostic_retained.context("retain worker protocol diagnostic")?;
            result
        }
        Err(error) => {
            anyhow::ensure!(confirmed, "worker protocol timeout; cleanup unconfirmed");
            Err(error.into())
        }
    }
}

#[tokio::test]
#[ignore = "requires independently attested PRX_C28 artifact and guarded runner"]
async fn worker_finishes_while_the_parent_still_holds_the_stdin_write_end() -> anyhow::Result<()> {
    let run = run_protocol_case(TestWorkerProtocolCase::HoldAfterThreeSteers).await?;
    assert!(
        run.status.success(),
        "worker must exit cleanly with the stdin write end still open, got {:?}",
        run.status
    );
    let result = assert_single_json_line(&run.stdout);
    assert!(!result.success, "the sealed run cannot succeed without a config source");
    let error = result.error.unwrap_or_default();
    assert!(
        error.contains(POST_AUTH_FAILURE),
        "the run must have passed authentication and started the stdin reader, got {error:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires independently attested PRX_C28 artifact and guarded runner"]
async fn worker_finishes_when_the_parent_closes_stdin_right_after_the_manifest() -> anyhow::Result<()> {
    let run = run_protocol_case(TestWorkerProtocolCase::CloseAfterManifest).await?;
    assert!(
        run.status.success(),
        "worker must exit cleanly after an early stdin close, got {:?}",
        run.status
    );
    let result = assert_single_json_line(&run.stdout);
    assert!(!result.success);
    assert!(
        result.error.unwrap_or_default().contains(POST_AUTH_FAILURE),
        "the early-close run must fail at the same post-authentication point"
    );
    Ok(())
}
