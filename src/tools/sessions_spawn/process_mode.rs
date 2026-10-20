use super::{
    HistoryEntry, OwnedProcessGroup, OwnedProcessPhase, PROCESS_MEMORY_STRATEGY_HYBRID,
    PROCESS_MEMORY_STRATEGY_ISOLATED, PROCESS_MEMORY_STRATEGY_SHARED, PROCESS_OWNER_PANICKED_PREFIX, ProcessRunControl,
    ProcessWorkerOutcome, SESSION_WORKER_CAP_ENV, SESSION_WORKER_CAP_EXPIRY_ENV, SpawnLineage, SpawnScope,
    WorkerSteerPipe, build_session_worker_cli_args, build_session_worker_manifest, cleanup_owned_child_after_panic,
    cleanup_process_worker_workspace, copy_dir_recursive, current_spawn_execution_context, describe_worker_exit,
    normalize_process_memory_strategy, private_worker_memory_db_path, process_terminal, recover_operator_interruption,
    recover_ordinary_interruption, run_spawned_child_lifecycle, session_worker_signing_key,
    shared_worker_memory_db_path, start_kill_and_reap_direct_child,
};
#[cfg(test)]
use super::{
    TestWorkerExecutable, TestWorkerRouteObservation, test_worker_cleanup_error_class, test_worker_process_identity,
};
use crate::config::AgentCompactionConfig;
use crate::memory::{MemoryEventRecording, MemoryFabric};
use crate::providers;
use anyhow::Context as _;
use futures_util::FutureExt;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(super) async fn run_sub_agent_process(
    run_id: &str,
    task: &str,
    provider_name: &str,
    model: &str,
    api_key: Option<&str>,
    route_hint: Option<&str>,
    route_source_provider: Option<&str>,
    route_credential: Option<&providers::credentials::CredentialRef>,
    route_uses_root_connection: bool,
    direct_uses_root_connection: bool,
    temperature: f64,
    temperature_override: Option<f64>,
    workspace_root: &std::path::Path,
    worker_workspace_root: &std::path::Path,
    agent_identity_dir: Option<&str>,
    agent_system_prompt: Option<&str>,
    allowed_tools: &[String],
    keep_workspace: bool,
    scope: Option<&SpawnScope>,
    spawn_depth: usize,
    session_scope_key: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    lineage: &SpawnLineage,
    memory_strategy: &str,
    memory_backend: &str,
    config_dir: &std::path::Path,
    config_generation: &str,
    event_recording: MemoryEventRecording,
    compaction_config: &AgentCompactionConfig,
    channel_excluded_tools: &[String],
    process_control: &ProcessRunControl,
    steer_rx: tokio::sync::mpsc::Receiver<String>,
    history: Arc<RwLock<Vec<HistoryEntry>>>,
    progress: Arc<crate::agent::idle::ProgressBeat>,
    provider_selection: Option<&crate::session_worker::protocol::WorkerProviderSelection>,
    source_fabric: Option<&MemoryFabric>,
    recovery: Option<&process_terminal::ProcessRecovery>,
    source_launch: Option<crate::runtime::operator_spawn::process::ProcessSourceLaunch>,
    batch_id: Option<&str>,
    #[cfg(test)] test_worker_executable: Option<&TestWorkerExecutable>,
) -> anyhow::Result<ProcessWorkerOutcome> {
    let process_generation = source_launch.as_ref().map(|launch| Arc::clone(launch.generation()));
    let ordinary_generation = source_launch
        .as_ref()
        .filter(|launch| launch.is_ordinary())
        .map(|launch| Arc::clone(launch.generation()));
    let operator_generation = source_launch
        .as_ref()
        .filter(|launch| !launch.is_ordinary())
        .map(|launch| Arc::clone(launch.generation()));
    anyhow::ensure!(
        operator_generation.is_none() || batch_id.is_none(),
        "operator process cannot claim model batch provenance"
    );
    let source_transport_key = source_launch.as_ref().map(|_| session_worker_signing_key());
    if let Some(launch) = &source_launch {
        let fabric = source_fabric.context("process source fabric missing")?;
        let issued = launch.source_fabric();
        anyhow::ensure!(
            fabric.workspace_id() == issued.workspace_id() && Arc::ptr_eq(&fabric.memory(), &issued.memory()),
            "process source storage differs from captured launch"
        );
    }
    let execution = current_spawn_execution_context();
    let child_source = execution
        .as_ref()
        .and_then(|context| context.incoming_grant.as_deref())
        .map(|grant| {
            let receipt = execution
                .as_ref()
                .and_then(|context| context.input_receipt.as_deref())
                .context("process child input receipt is missing")?;
            grant.verify_receipt(receipt)?;
            let selection = provider_selection.context("process child provider selection missing")?;
            Ok::<_, anyhow::Error>((grant, receipt, selection))
        })
        .transpose()?;
    let logical_workspace_root = child_source.as_ref().map_or_else(
        || {
            process_generation.as_ref().map_or(workspace_root, |generation| {
                generation.effective.workspace_dir.as_path()
            })
        },
        |(grant, _, _)| grant.generation().effective.workspace_dir.as_path(),
    );
    if let Some((grant, _, _)) = child_source.as_ref() {
        let fabric = source_fabric.context("process child logical source fabric missing")?;
        anyhow::ensure!(
            fabric.workspace_id() == grant.source_fabric().workspace_id()
                && Arc::ptr_eq(&fabric.memory(), &grant.source_fabric().memory()),
            "process child source storage differs from issued grant"
        );
    }
    let worker_workspace = worker_workspace_root.join(run_id);
    #[cfg(all(test, unix))]
    {
        if let Some(artifact) = test_worker_executable.filter(|artifact| artifact.protocol.is_some()) {
            use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
            artifact.verify()?;
            let home = std::fs::canonicalize(&artifact.private_home)?;
            let root = std::fs::canonicalize(worker_workspace_root)?;
            anyhow::ensure!(
                root != home && root.starts_with(&home),
                "protocol workspace root must be inside owned private HOME"
            );
            for path in [&home, &root] {
                anyhow::ensure!(
                    std::fs::symlink_metadata(path)?.permissions().mode() & 0o777 == 0o700,
                    "protocol HOME and workspace root must be private"
                );
            }
            std::fs::DirBuilder::new().mode(0o700).create(&worker_workspace)?;
            anyhow::ensure!(
                std::fs::symlink_metadata(&worker_workspace)?.permissions().mode() & 0o777 == 0o700,
                "protocol worker workspace must be private at creation"
            );
        } else {
            std::fs::create_dir_all(&worker_workspace)?;
        }
    }
    #[cfg(any(not(test), not(unix)))]
    std::fs::create_dir_all(&worker_workspace)?;
    let shared_memory_db_path = shared_worker_memory_db_path(logical_workspace_root);
    let worker_memory_db_path = private_worker_memory_db_path(&worker_workspace);
    let normalized_memory_strategy = normalize_process_memory_strategy(memory_strategy)?;
    let (memory_db_path, memory_workspace_id) = match normalized_memory_strategy {
        PROCESS_MEMORY_STRATEGY_SHARED => (
            shared_memory_db_path.clone(),
            logical_workspace_root.to_string_lossy().to_string(),
        ),
        PROCESS_MEMORY_STRATEGY_ISOLATED | PROCESS_MEMORY_STRATEGY_HYBRID => (
            worker_memory_db_path.clone(),
            worker_workspace.to_string_lossy().to_string(),
        ),
        other => anyhow::bail!("invalid normalized process memory strategy '{other}'"),
    };

    let identity_dir = if let Some(identity_dir) = agent_identity_dir {
        let source_identity = workspace_root.join(identity_dir);
        if source_identity.exists() {
            let copied_identity_dir = worker_workspace.join("identity");
            if source_identity.is_dir() {
                copy_dir_recursive(&source_identity, &copied_identity_dir)?;
            } else {
                std::fs::create_dir_all(&copied_identity_dir)?;
                let target = copied_identity_dir.join(
                    source_identity
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new("identity.txt")),
                );
                std::fs::copy(source_identity, target)?;
            }
            Some("identity".to_string())
        } else {
            None
        }
    } else {
        None
    };

    // FIX-P0-36: build the manifest with an empty capability first, then seal
    // it with an HMAC bound to the run id, an absolute expiry, and a digest of
    // the manifest contents. A leaked capability token therefore cannot be
    // replayed for a different run, after expiry, or with a tampered manifest.
    let (manifest, sealed_capability, capability_expiry) = build_session_worker_manifest(
        run_id,
        task,
        provider_name,
        model,
        api_key,
        route_hint,
        route_source_provider,
        route_credential,
        route_uses_root_connection,
        direct_uses_root_connection,
        temperature,
        temperature_override,
        worker_workspace.clone(),
        memory_db_path,
        memory_workspace_id,
        normalized_memory_strategy,
        memory_backend,
        shared_memory_db_path,
        worker_memory_db_path,
        config_dir.to_path_buf(),
        config_generation,
        agent_id,
        agent_system_prompt,
        event_recording,
        allowed_tools,
        identity_dir,
        scope,
        lineage,
        spawn_depth,
        session_scope_key,
        parent_run_id,
        compaction_config,
        channel_excluded_tools,
        child_source,
        source_launch.zip(source_transport_key.as_ref()),
    )?;

    if let Some(reason) = process_control.termination_reason() {
        cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
        return Ok(ProcessWorkerOutcome::Terminated(reason));
    }

    #[cfg(not(test))]
    let executable = std::env::current_exe()?;
    #[cfg(test)]
    let executable = if let Some(artifact) = test_worker_executable {
        artifact.verify()?;
        artifact.path.clone()
    } else {
        std::env::current_exe()?
    };
    #[cfg(test)]
    if let Some(observer) = test_worker_executable.and_then(|artifact| artifact.route_observer.as_ref()) {
        use sha2::{Digest as _, Sha256};
        let mut capture = observer.lock();
        capture.total = capture.total.saturating_add(1);
        if capture.first.is_none() {
            capture.first = Some(TestWorkerRouteObservation {
                run_id: manifest.run_id.clone(),
                parent_run_id: manifest.parent_run_id.clone(),
                owner_id: manifest.owner_id.clone(),
                source_event_id: manifest.source_message_event_id.clone(),
                provider: manifest.provider_name.clone(),
                model: manifest.model.clone(),
                route_hint: manifest.route_hint.clone(),
                route_source_provider: manifest.route_source_provider.clone(),
                route_credential: manifest.route_credential.clone(),
                key_sha256: manifest
                    .api_key
                    .as_ref()
                    .map(|key| Sha256::digest(key.as_bytes()).into()),
                route_uses_root_connection: manifest.route_uses_root_connection,
                direct_uses_root_connection: manifest.direct_uses_root_connection,
                child_source_present: manifest.child_source.is_some(),
                child_allowed_count: manifest.capability_manifest.as_ref().map(|scope| scope.allowed.len()),
            });
        }
    }
    let cli_args = build_session_worker_cli_args(&manifest)?;
    let mut command = tokio::process::Command::new(executable);
    #[cfg(test)]
    if let Some(artifact) = test_worker_executable {
        // The outer guarded runner remains responsible for OS filesystem/network
        // confinement. Do not launch this ignored AUT outside that guard.
        command
            .env_clear()
            .env("HOME", &artifact.private_home)
            .env("PATH", "/usr/bin:/bin");
    }
    command.env(SESSION_WORKER_CAP_ENV, &sealed_capability);
    command.env(SESSION_WORKER_CAP_EXPIRY_ENV, capability_expiry.to_string());
    command
        .args(cli_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    #[cfg(unix)]
    let mut prepared_manifest = crate::session_worker::manifest_reference::PreparedManifest::prepare(
        &manifest,
        &source_transport_key.clone().unwrap_or_else(session_worker_signing_key),
        capability_expiry,
    )?;
    #[cfg(unix)]
    let payload = prepared_manifest.line().to_owned();
    #[cfg(not(unix))]
    let payload = serde_json::to_string(&manifest)?;
    #[cfg(test)]
    let protocol = test_worker_executable.and_then(|artifact| artifact.protocol.as_deref());
    #[cfg(test)]
    if let Some(control) = protocol {
        control.claim()?;
        control.check_serialized_precondition(&payload)?;
    }
    let mut child = command.spawn()?;
    #[cfg(unix)]
    prepared_manifest.mark_spawned();
    #[cfg(test)]
    if let Some(control) = protocol {
        let mut diagnostic = control.diagnostic.lock();
        diagnostic.spawned = true;
        diagnostic.pid = child.id();
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let (pgid, start, class) = test_worker_process_identity(pid);
            diagnostic.actual_pgid = pgid;
            diagnostic.start_identity = start;
            diagnostic.identity_class = Some(class);
        }
    }
    // Ledger row for the worker process itself, carrying the process group so a
    // kill reaches anything the worker forks. Its parent resolves to the
    // sub-agent row, because this future runs inside that row's scope.
    #[cfg(unix)]
    let worker_pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0);
    #[cfg(not(unix))]
    let worker_pgid: Option<i32> = None;
    let mut worker_registration =
        crate::runtime::registry::register_process(&format!("session-worker {run_id}"), child.id(), worker_pgid);
    #[cfg(test)]
    if let Some(control) = protocol {
        control.diagnostic.lock().registry_id = Some(worker_registration.id().raw());
    }
    let mut process_group = match OwnedProcessGroup::from_child(&child) {
        Ok(group) => group,
        Err(error) => {
            let status = start_kill_and_reap_direct_child(&mut child)
                .await
                .map_err(|cleanup_error| anyhow::anyhow!("{error}; direct-child cleanup failed: {cleanup_error}"))?;
            #[cfg(not(test))]
            let _ = status;
            #[cfg(test)]
            if let Some(control) = protocol {
                control.observe_reap_status(status);
            }
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close().map_err(|cleanup_error| {
                anyhow::anyhow!(
                    "session-worker process group setup failed: {error}; snapshot cleanup failed: {cleanup_error}"
                )
            })?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
            return Err(error);
        }
    };
    #[cfg(test)]
    if let Some(control) = protocol {
        process_group.termination_calls = Some(Arc::clone(&control.termination_calls));
        process_group.protocol_diagnostic = Some(Arc::clone(&control.diagnostic));
    }
    let owned_phase = std::panic::AssertUnwindSafe(async {
        if let Some(recovery) = recovery {
            recovery
                .persist_launch(child.id().context("worker process id unavailable before launch")?)
                .await?;
        }
        if let Some(generation) = &ordinary_generation {
            process_terminal::ordinary::launch::persist(
                source_fabric.context("ordinary launch source storage missing")?,
                &manifest,
                generation,
                source_transport_key
                    .as_ref()
                    .context("ordinary launch transport key missing")?,
                child.id().context("ordinary worker PID unavailable before launch")?,
                batch_id,
            )
            .await?;
        }
        if let Some(generation) = &operator_generation {
            process_terminal::operator::launch::persist(
                source_fabric.context("operator launch source storage missing")?,
                &manifest,
                generation,
                source_transport_key
                    .as_ref()
                    .context("operator launch transport key missing")?,
                child.id().context("operator worker PID unavailable before launch")?,
            )
            .await?;
        }
        #[cfg(test)]
        if let Some(control) = protocol {
            if operator_generation.is_some() {
                control.diagnostic.lock().launch_persisted = true;
            }
            anyhow::ensure!(
                operator_generation.is_some()
                    && manifest.operator_source.is_some()
                    && manifest.memory_event_recording.enabled
                    && manifest.memory_event_recording.record_user_messages,
                "stdin protocol control requires a recorded operator launch"
            );
            let artifact = test_worker_executable.context("worker protocol artifact missing")?;
            let home = std::fs::canonicalize(&artifact.private_home)?;
            let config_dir = std::fs::canonicalize(&manifest.config_dir)?;
            anyhow::ensure!(
                config_dir != home && config_dir.starts_with(&home),
                "protocol config must be inside owned private HOME"
            );
            let config_path = config_dir.join("config.toml");
            anyhow::ensure!(
                std::fs::symlink_metadata(&config_path)?.file_type().is_file(),
                "protocol config must be a regular file"
            );
            std::fs::remove_file(config_path)?;
        }
        run_spawned_child_lifecycle(
            &mut child,
            &mut process_group,
            &payload,
            process_control,
            Some(WorkerSteerPipe {
                steer_rx,
                history,
                run_id: run_id.to_string(),
            }),
            Some(progress),
            #[cfg(test)]
            false,
            #[cfg(test)]
            protocol,
        )
        .await
    })
    .catch_unwind()
    .await;
    // Confirm the leader's reap only on branches with actual cleanup evidence.
    // Failed cleanup keeps the process ledger row and workspace available for
    // recovery; a returned lifecycle result alone is not acknowledgement.
    let owned_phase = match owned_phase {
        Ok(Ok(phase)) => phase,
        Ok(Err(error)) => {
            let cleanup = cleanup_owned_child_after_panic(
                &mut child,
                &mut process_group,
                #[cfg(test)]
                false,
            )
            .await;
            #[cfg(test)]
            if let Some(control) = protocol {
                let mut diagnostic = control.diagnostic.lock();
                diagnostic.lifecycle_class = Some("lifecycle_error");
                diagnostic.panic_cleanup_class = Some(match &cleanup {
                    Ok(()) => "cleanup_returned_ok",
                    Err(error) => test_worker_cleanup_error_class(error),
                });
            }
            cleanup.map_err(|cleanup_error| {
                anyhow::anyhow!("session-worker lifecycle failed: {error}; cleanup failed: {cleanup_error}")
            })?;
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close().map_err(|cleanup_error| {
                anyhow::anyhow!("session-worker lifecycle failed: {error}; snapshot cleanup failed: {cleanup_error}")
            })?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
            return Err(error);
        }
        Err(_) => {
            let cleanup = cleanup_owned_child_after_panic(
                &mut child,
                &mut process_group,
                #[cfg(test)]
                false,
            )
            .await;
            #[cfg(test)]
            if let Some(control) = protocol {
                let mut diagnostic = control.diagnostic.lock();
                diagnostic.lifecycle_class = Some("monitor_panic");
                diagnostic.panic_cleanup_class = Some(match &cleanup {
                    Ok(()) => "cleanup_returned_ok",
                    Err(error) => test_worker_cleanup_error_class(error),
                });
            }
            cleanup.map_err(|error| anyhow::anyhow!("{PROCESS_OWNER_PANICKED_PREFIX}; cleanup failed: {error}"))?;
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close().map_err(|cleanup_error| {
                anyhow::anyhow!("{PROCESS_OWNER_PANICKED_PREFIX}; snapshot cleanup failed: {cleanup_error}")
            })?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
            anyhow::bail!("{PROCESS_OWNER_PANICKED_PREFIX} while owning session-worker child");
        }
    };
    let (status, stdout, stderr) = match owned_phase {
        OwnedProcessPhase::Exited { status, stdout, stderr } => {
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close()?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            (status, stdout, stderr)
        }
        OwnedProcessPhase::Terminated(reason) => {
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close()?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            if let Some(generation) = &ordinary_generation {
                let recovered = recover_ordinary_interruption(
                    source_fabric.context("ordinary interrupted source storage missing")?,
                    &manifest,
                    generation,
                    source_transport_key
                        .as_ref()
                        .context("ordinary interruption transport key missing")?,
                    reason,
                )
                .await;
                cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
                return recovered;
            }
            if let Some(generation) = &operator_generation {
                let recovered = recover_operator_interruption(
                    source_fabric.context("operator interrupted source storage missing")?,
                    &manifest,
                    generation,
                    source_transport_key
                        .as_ref()
                        .context("operator interruption transport key missing")?,
                    reason,
                )
                .await;
                cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
                return recovered;
            }
            cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
            return Ok(ProcessWorkerOutcome::Terminated(reason));
        }
        OwnedProcessPhase::TerminationFailed(error) => {
            tracing::warn!(
                run_id,
                "worker cleanup unconfirmed; retaining process record and workspace"
            );
            return Ok(ProcessWorkerOutcome::TerminationFailed(error));
        }
    };
    let stdout_raw = String::from_utf8_lossy(&stdout).trim().to_string();
    let stderr_raw = String::from_utf8_lossy(&stderr).trim().to_string();
    let outcome = classify_worker_output(status, &stdout_raw, &stderr_raw);
    #[cfg(all(test, unix))]
    if let (Some(artifact), Ok(ProcessWorkerOutcome::Finished(result))) = (test_worker_executable, &outcome) {
        if !result.success && result.completion_evidence.is_none() {
            if let Some(error) = result.error.as_deref() {
                // Diagnostic only for the explicitly attested local AUT. Keep
                // unsigned failure evidence private; it grants no authority.
                let retained = (|| -> anyhow::Result<()> {
                    use std::io::Write as _;
                    use std::os::unix::fs::OpenOptionsExt as _;
                    anyhow::ensure!(
                        std::fs::symlink_metadata(&artifact.private_home)?.file_type().is_dir(),
                        "worker diagnostic home is not an owned directory"
                    );
                    let redacted = crate::agent::terminal::redact_persisted_text(error);
                    let bounded: String = redacted.chars().take(4096).collect();
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(artifact.private_home.join("unsigned-worker-error.txt"))?;
                    file.write_all(bounded.as_bytes())?;
                    file.sync_all()?;
                    Ok(())
                })();
                if retained.is_err() {
                    tracing::warn!("owned worker unsigned-error diagnostic could not be retained");
                }
            }
        }
    }
    let outcome = async {
        // A cleanup-confirmed worker can exit after journaling but before its
        // stdout result reaches the parent. Reuse its exact launch authority.
        if let Some(generation) = &ordinary_generation {
            let interruption = match &outcome {
                Ok(ProcessWorkerOutcome::ExitedWithoutResult(detail)) => Some(detail.clone()),
                Err(_) => Some("worker output was invalid or conflicted with its exit status".to_owned()),
                _ => None,
            };
            if let Some(detail) = interruption {
                return recover_ordinary_interruption(
                    source_fabric.context("ordinary interrupted source storage missing")?,
                    &manifest,
                    generation,
                    source_transport_key
                        .as_ref()
                        .context("ordinary interruption transport key missing")?,
                    detail,
                )
                .await;
            }
        }
        if let Some(generation) = &operator_generation {
            let interruption = match &outcome {
                Ok(ProcessWorkerOutcome::ExitedWithoutResult(detail)) => Some(detail.clone()),
                Err(_) => Some("worker output was invalid or conflicted with its exit status".to_owned()),
                _ => None,
            };
            if let Some(detail) = interruption {
                return recover_operator_interruption(
                    source_fabric.context("operator interrupted source storage missing")?,
                    &manifest,
                    generation,
                    source_transport_key
                        .as_ref()
                        .context("operator interruption transport key missing")?,
                    detail,
                )
                .await;
            }
        }
        match outcome? {
            ProcessWorkerOutcome::UnverifiedStartupFailure(envelope) => {
                anyhow::ensure!(
                    child_source.is_none() && ordinary_generation.is_none(),
                    "startup failure requires the original operator launch"
                );
                let generation = operator_generation
                    .as_ref()
                    .context("operator startup generation missing")?;
                let key = source_transport_key
                    .as_ref()
                    .context("operator startup transport key missing")?;
                // Authenticate the retained launch snapshot, not mutable post-launch disk state.
                process_terminal::operator::verify_startup_failure(
                    source_fabric.context("operator startup source storage missing")?,
                    &manifest,
                    generation,
                    key,
                    &envelope,
                )
                .await?;
                // This branch is reached only after OwnedProcessPhase::Exited and
                // mark_reaped above; terminated/unconfirmed owners return earlier.
                Ok(ProcessWorkerOutcome::VerifiedStartupFailure(envelope.result))
            }
            ProcessWorkerOutcome::Finished(result) => {
                if let Some((grant, input, _)) = child_source {
                    anyhow::ensure!(
                        input.event().run_id.as_deref() == Some(run_id),
                        "worker result belongs to another launched run"
                    );
                    let verified = process_terminal::verify(
                        source_fabric.context("worker logical source storage missing")?,
                        grant,
                        input,
                        result,
                    )
                    .await?;
                    Ok(ProcessWorkerOutcome::VerifiedFinished(verified))
                } else if let Some(generation) = &ordinary_generation {
                    let mut verified = process_terminal::ordinary::verify(
                        source_fabric.context("ordinary process source storage missing")?,
                        &manifest,
                        generation,
                        source_transport_key
                            .as_ref()
                            .context("ordinary process transport key missing")?,
                        result,
                    )
                    .await?;
                    verified.confirm_reaped(run_id)?;
                    Ok(ProcessWorkerOutcome::OrdinaryVerifiedFinished(verified))
                } else if let Some(generation) = &operator_generation {
                    let mut verified = process_terminal::operator::verify(
                        source_fabric.context("operator process source storage missing")?,
                        &manifest,
                        generation,
                        source_transport_key
                            .as_ref()
                            .context("operator process transport key missing")?,
                        result,
                    )
                    .await?;
                    verified.confirm_reaped(run_id)?;
                    Ok(ProcessWorkerOutcome::OperatorVerifiedFinished(verified))
                } else {
                    anyhow::ensure!(
                        result.completion_evidence.is_none()
                            && result.ordinary_completion.is_none()
                            && result.operator_completion.is_none(),
                        "ordinary worker completion requires its authenticated verifier"
                    );
                    Ok(ProcessWorkerOutcome::Finished(result))
                }
            }
            other => Ok(other),
        }
    }
    .await;
    // The leader is confirmed reaped here, including decode/verification
    // failures. Unconfirmed cleanup branches above retain their workspace.
    cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
    outcome
}

/// Turn "the worker process is gone, here is what it left on its pipes" into a
/// run outcome.
///
/// Split out from [`run_sub_agent_process`] because this is the whole of the
/// process-mode liveness contract and it must be testable against a real
/// SIGKILLed child without standing up a manifest, a workspace and a config
/// directory first.
pub(super) fn classify_worker_output(
    status: std::process::ExitStatus,
    stdout_raw: &str,
    stderr_raw: &str,
) -> anyhow::Result<ProcessWorkerOutcome> {
    // EOF without a result line. A closed pipe is the *only* thing the parent
    // observes when the worker is OOM-killed, SIGKILLed, or segfaults, so it has
    // to be mapped to a terminal state right here — no later event is coming,
    // and there is no wall clock in this runtime that would eventually give up.
    //
    // MUTATION GUARD: delete this branch and a killed worker falls through to
    // the decode error below, which is still terminal but no longer names the
    // fault; delete the whole classification and the run stays `Running`
    // forever, which is the bug this exists for.
    if stdout_raw.is_empty() {
        return Ok(ProcessWorkerOutcome::ExitedWithoutResult(describe_worker_exit(
            status, stderr_raw,
        )));
    }

    let envelope: crate::session_worker::startup_failure::WorkerOutput =
        serde_json::from_str(stdout_raw).map_err(|error| {
            anyhow::anyhow!(
                "Failed to parse session-worker output: {error}; status={:?}; stderr={stderr_raw}",
                status.code(),
            )
        })?;

    if envelope.authenticated_startup_failure.is_some() {
        anyhow::ensure!(status.success(), "startup failure envelope requires clean owned exit");
        return Ok(ProcessWorkerOutcome::UnverifiedStartupFailure(envelope));
    }
    let parsed = envelope.result;
    if !status.success() && parsed.success {
        return Err(anyhow::anyhow!(
            "session-worker exited with status {:?} despite success result",
            status.code()
        ));
    }

    Ok(ProcessWorkerOutcome::Finished(parsed))
}
