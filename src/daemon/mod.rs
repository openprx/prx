#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::config::{Config, HotReloadManager, new_shared};
use crate::self_system::evolution::{
    AsyncJsonlWriter, EvolutionAnalyzer, EvolutionConfig, EvolutionPipeline, EvolutionRetentionConfig,
    EvolutionRuntimeConfig, EvolutionScheduler, JsonlRetentionPolicy, JsonlStoragePaths, MemoryEvolutionEngine,
    PromptEvolutionEngine, StrategyEvolutionEngine, new_shared_evolution_config,
};
use anyhow::{Context, Result};
use chrono::Utc;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

const STATUS_FLUSH_SECONDS: u64 = 5;
const CORE_HEALTH_TTL_SECONDS: u64 = 60;
const OPTIONAL_HEALTH_TTL_SECONDS: u64 = 300;

pub async fn run(config: Config, host: String, port: u16, shutdown: CancellationToken) -> Result<()> {
    // Single-instance mutual exclusion. The guard must stay alive for the whole
    // function: dropping it closes the descriptor and releases the flock.
    let _instance_lock = acquire_single_instance_lock(&config)?;

    crate::health::register_component(
        "daemon",
        "daemon",
        true,
        Duration::from_secs(CORE_HEALTH_TTL_SECONDS),
        crate::health::ComponentState::Starting,
    );

    let initial_backoff = config.reliability.channel_initial_backoff_secs.max(1);
    let max_backoff = config.reliability.channel_max_backoff_secs.max(initial_backoff);

    // Build the sole process configuration owner before assembling participants.
    let shared_config = new_shared(config.clone());

    let startup_trace =
        crate::runtime::control_ladder::ControlLadderSnapshot::from_config(&config).build_trace("daemon.start", None);
    if let Err(error) =
        crate::runtime::control_ladder::append_control_ladder_trace(&config.workspace_dir, &startup_trace)
    {
        tracing::warn!("failed to append daemon control ladder trace: {error}");
    }

    let _ = crate::heartbeat::engine::HeartbeatEngine::ensure_heartbeat_file(&config.workspace_dir).await;

    // Skill lifecycle recovery sweep (no-op unless `[skills_lifecycle]` is enabled).
    crate::skills::lifecycle_runtime::spawn_startup_recovery(&config);

    let mut handles: Vec<JoinHandle<()>> = vec![spawn_state_writer(config.clone(), Arc::clone(&shared_config))];

    // Gateway is an always-available runtime capability.
    {
        crate::health::register_component(
            "gateway",
            "gateway",
            true,
            Duration::from_secs(CORE_HEALTH_TTL_SECONDS),
            crate::health::ComponentState::Starting,
        );
        let gateway_host = host.clone();
        let gateway_shared = Arc::clone(&shared_config);
        handles.push(spawn_component_supervisor(
            "gateway",
            initial_backoff,
            max_backoff,
            move || {
                let host = gateway_host.clone();
                let shared = Arc::clone(&gateway_shared);
                async move {
                    let cfg = (*shared.load_full()).clone();
                    // Supervised gateway: the supervisor restarts it on exit, so
                    // it uses a never-cancelled token. See never_cancelled_shutdown.
                    crate::gateway::run_gateway(
                        &host,
                        port,
                        cfg,
                        Some(shared),
                        crate::runtime::shutdown::never_cancelled_shutdown(),
                    )
                    .await
                }
            },
        ));
    }

    // Retained outside abortable component supervisors and their spawned jobs.
    let runtime_cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let initial_generation = shared_config.pin();
    let mut generation_controllers = Vec::new();

    let (channels_handle, channels_controller) = spawn_config_generation_supervisor(
        "channels",
        "channels",
        OPTIONAL_HEALTH_TTL_SECONDS,
        Arc::clone(&initial_generation),
        Arc::new(channels_prepares_for_field),
        Arc::new(has_supervised_channels),
        {
            let shared = Arc::clone(&shared_config);
            let cleanup_owner = Arc::clone(&runtime_cleanup);
            let retirement_gate = Arc::new(tokio::sync::Mutex::new(()));
            Arc::new(move |generation, shutdown| {
                let shared = Arc::clone(&shared);
                let cleanup_owner = Arc::clone(&cleanup_owner);
                let retirement_gate = Arc::clone(&retirement_gate);
                Box::pin(async move {
                    let retirement = retirement_gate.lock_owned().await;
                    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(
                        Some(&cleanup_owner),
                        CancellationToken::new(),
                    )?
                    .with_retirement_guard(retirement);
                    let work: Pin<Box<dyn Future<Output = Result<()>> + Send>> =
                        Box::pin(crate::channels::start_channels_with_config(
                            (*generation.effective).clone(),
                            shared,
                            generation,
                            shutdown,
                        ));
                    cleanup.run(work).await
                })
            })
        },
        shutdown.clone(),
    );
    handles.push(channels_handle);
    generation_controllers.push(channels_controller);

    let scheduler_ttl = config
        .reliability
        .scheduler_poll_secs
        .saturating_mul(3)
        .max(OPTIONAL_HEALTH_TTL_SECONDS);
    let (scheduler_handle, scheduler_controller) = spawn_config_generation_supervisor(
        "scheduler",
        "cron",
        scheduler_ttl,
        Arc::clone(&initial_generation),
        Arc::new(|field| {
            matches!(field, "cron" | "scheduler") || crate::config::generation::is_rebuild_and_swap(field)
        }),
        Arc::new(|_| true),
        {
            let shared = Arc::clone(&shared_config);
            let cleanup_owner = Arc::clone(&runtime_cleanup);
            let retirement_gate = Arc::new(tokio::sync::Mutex::new(()));
            Arc::new(move |generation, shutdown| {
                let shared = Arc::clone(&shared);
                let cleanup_owner = Arc::clone(&cleanup_owner);
                let retirement_gate = Arc::clone(&retirement_gate);
                Box::pin(async move {
                    let retirement = retirement_gate.lock_owned().await;
                    let cleanup =
                        crate::runtime::turn_cleanup::CleanupScope::new(Some(&cleanup_owner), shutdown.clone())?
                            .with_retirement_guard(retirement);
                    let work: Pin<Box<dyn Future<Output = Result<()>> + Send>> = Box::pin(async move {
                        let cfg = (*generation.effective).clone();
                        tokio::select! {
                            result = crate::cron::scheduler::run_with_config_generation_manager(
                                cfg,
                                generation,
                                shared,
                            ) => result,
                            () = shutdown.cancelled() => Ok(()),
                        }
                    });
                    cleanup.run(work).await
                })
            })
        },
        shutdown.clone(),
    );
    handles.push(scheduler_handle);
    generation_controllers.push(scheduler_controller);

    let xin_ttl = u64::from(crate::xin::runner::runner_interval_minutes(&config))
        .saturating_mul(120)
        .max(OPTIONAL_HEALTH_TTL_SECONDS);
    let (xin_handle, xin_controller) = spawn_config_generation_supervisor(
        "xin",
        "xin",
        xin_ttl,
        Arc::clone(&initial_generation),
        Arc::new(|field| {
            matches!(field, "xin" | "heartbeat" | "self_system")
                || crate::config::generation::is_rebuild_and_swap(field)
        }),
        Arc::new(xin_runtime_enabled),
        {
            let shared = Arc::clone(&shared_config);
            let cleanup_owner = Arc::clone(&runtime_cleanup);
            let retirement_gate = Arc::new(tokio::sync::Mutex::new(()));
            Arc::new(move |generation, shutdown| {
                let shared = Arc::clone(&shared);
                let cleanup_owner = Arc::clone(&cleanup_owner);
                let retirement_gate = Arc::clone(&retirement_gate);
                Box::pin(async move {
                    let retirement = retirement_gate.lock_owned().await;
                    let cleanup =
                        crate::runtime::turn_cleanup::CleanupScope::new(Some(&cleanup_owner), shutdown.clone())?
                            .with_retirement_guard(retirement);
                    let work: Pin<Box<dyn Future<Output = Result<()>> + Send>> = Box::pin(async move {
                        let cfg = (*generation.effective).clone();
                        tokio::select! {
                            result = crate::xin::runner::run_with_config_generation_manager(
                                cfg,
                                generation,
                                shared,
                            ) => result,
                            () = shutdown.cancelled() => Ok(()),
                        }
                    });
                    cleanup.run(work).await
                })
            })
        },
        shutdown.clone(),
    );
    handles.push(xin_handle);
    generation_controllers.push(xin_controller);

    let fitness_ttl = 48_u64.saturating_mul(3600).max(OPTIONAL_HEALTH_TTL_SECONDS);
    let (fitness_handle, fitness_controller) = spawn_config_generation_supervisor(
        "self_system_fitness",
        "self-system",
        fitness_ttl,
        Arc::clone(&initial_generation),
        Arc::new(|field| field == "self_system"),
        Arc::new(|config| config.self_system.fitness.auto_run),
        {
            let shared = Arc::clone(&shared_config);
            Arc::new(move |generation, shutdown| {
                let shared = Arc::clone(&shared);
                Box::pin(async move {
                    tokio::select! {
                        result = run_fitness_worker(
                            (*generation.effective).clone(),
                            shared,
                            generation.id,
                        ) => result,
                        () = shutdown.cancelled() => Ok(()),
                    }
                })
            })
        },
        shutdown.clone(),
    );
    handles.push(fitness_handle);
    generation_controllers.push(fitness_controller);

    let (evolution_handle, evolution_controller) = spawn_config_generation_supervisor(
        "evolution_scheduler",
        "self-system",
        fitness_ttl,
        Arc::clone(&initial_generation),
        Arc::new(|field| field == "self_system" || crate::config::generation::is_rebuild_and_swap(field)),
        Arc::new(|_| true),
        {
            let shared = Arc::clone(&shared_config);
            let cleanup_owner = Arc::clone(&runtime_cleanup);
            let retirement_gate = Arc::new(tokio::sync::Mutex::new(()));
            Arc::new(move |generation, shutdown| {
                let shared = Arc::clone(&shared);
                let cleanup_owner = Arc::clone(&cleanup_owner);
                let retirement_gate = Arc::clone(&retirement_gate);
                Box::pin(async move {
                    let retirement = retirement_gate.lock_owned().await;
                    let cleanup =
                        crate::runtime::turn_cleanup::CleanupScope::new(Some(&cleanup_owner), shutdown.clone())?
                            .with_retirement_guard(retirement);
                    cleanup
                        .run(async move {
                            tokio::select! {
                                result = run_evolution_scheduler_worker(
                                    (*generation.effective).clone(),
                                    shared,
                                    generation.id,
                                ) => result,
                                () = shutdown.cancelled() => Ok(()),
                            }
                        })
                        .await
                })
            })
        },
        shutdown.clone(),
    );
    handles.push(evolution_handle);
    generation_controllers.push(evolution_controller);

    let (webhook_handle, webhook_controller) = spawn_config_generation_supervisor(
        "webhook_receiver",
        "webhook",
        OPTIONAL_HEALTH_TTL_SECONDS,
        initial_generation,
        Arc::new(|field| field == "webhook" || crate::config::generation::is_rebuild_and_swap(field)),
        Arc::new(|cfg| cfg.webhook.configured()),
        {
            let shared = Arc::clone(&shared_config);
            Arc::new(move |generation, shutdown| {
                let shared = Arc::clone(&shared);
                Box::pin(async move {
                    let cfg = (*generation.effective).clone();
                    let repository = crate::webhook::repository_from_config(&cfg)?;
                    let security = crate::runtime::bootstrap::build_security_policy(&cfg);
                    tokio::select! {
                        result = crate::webhook::run_configured_with_repository_generation(
                            &cfg,
                            repository,
                            security,
                            shared,
                            generation.id,
                        ) => result,
                        () = shutdown.cancelled() => Ok(()),
                    }
                })
            })
        },
        shutdown.clone(),
    );
    handles.push(webhook_handle);
    generation_controllers.push(webhook_controller);

    let daemon_participant: Arc<dyn crate::config::ConfigGenerationParticipant> =
        Arc::new(DaemonConfigGenerationParticipant {
            controllers: generation_controllers,
        });
    shared_config.register_participant(&daemon_participant);
    // Start disk observation only after controlled-runtime participants exist,
    // so the first candidate cannot publish into an owner with no supervisor.
    let _hot_reload = HotReloadManager::spawn(config.config_path.clone(), Arc::clone(&shared_config));

    let heartbeat_ttl = u64::from(config.heartbeat.interval_minutes)
        .saturating_mul(120)
        .max(OPTIONAL_HEALTH_TTL_SECONDS);
    register_optional_component("heartbeat", "xin", true, heartbeat_ttl);

    println!("🧠 OpenPRX daemon started");
    println!("   Gateway:  http://{host}:{port}");
    println!(
        "   Components: gateway, channels, heartbeat, scheduler, xin, self_system_fitness, evolution_scheduler, webhook_receiver"
    );
    println!("   Ctrl+C to stop");
    crate::health::mark_component_ok("daemon");

    let stopped_during_startup = tokio::select! {
        () = crate::health::wait_until_ready() => false,
        () = shutdown.cancelled() => true,
        res = tokio::signal::ctrl_c() => {
            res?;
            true
        },
    };
    if !stopped_during_startup {
        systemd_notify::ready();

        // D5/D9 step 4: wait for either the external root shutdown token (e.g. the
        // dispatch-owned signal task wired in A6) or a direct ctrl_c as a fallback.
        // Supervisors still use abort-based teardown. Decision terminal cleanup
        // has a separate bounded acknowledgement below, like evolution commits.
        tokio::select! {
            () = shutdown.cancelled() => {}
            res = tokio::signal::ctrl_c() => res?,
        }
    }
    crate::health::mark_component_stopping("daemon");
    systemd_notify::stopping();

    for handle in &handles {
        handle.abort();
    }
    for handle in handles {
        let _ = handle.await;
    }
    // D-046: an evolution verdict task may still be judging or recording
    // its cycle; the aborts above do not reach it. Cancel it and wait,
    // bounded, so it records the cycle before `main` shuts the runtime down.
    let runtime_cleanup_result = runtime_cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(30))
        .await;
    drain_evolution_commits(crate::self_system::evolution::commit::daemon_commits()).await;
    runtime_cleanup_result?;
    crate::health::mark_component_stopped("daemon");

    Ok(())
}

fn register_optional_component(component: &str, owner: &str, enabled: bool, freshness_ttl_seconds: u64) {
    // Background capabilities are observable but not core ingress prerequisites:
    // their failure degrades the snapshot without preventing gateway readiness.
    crate::health::register_component(
        component,
        owner,
        false,
        Duration::from_secs(freshness_ttl_seconds.max(1)),
        if enabled {
            crate::health::ComponentState::Starting
        } else {
            crate::health::ComponentState::Disabled
        },
    );
}

pub fn state_file_path(config: &Config) -> PathBuf {
    config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("daemon_state.json")
}

fn spawn_state_writer(config: Config, shared_config: crate::config::SharedConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        let path = state_file_path(&config);
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                tracing::warn!("Failed to create daemon state directory: {e}");
            }
        }

        let mut interval = tokio::time::interval(Duration::from_secs(STATUS_FLUSH_SECONDS));
        loop {
            interval.tick().await;
            crate::health::touch_component("daemon");
            let mut json = crate::health::snapshot_json();
            if let Some(obj) = json.as_object_mut() {
                obj.insert("written_at".into(), serde_json::json!(Utc::now().to_rfc3339()));
                // The daemon's own binary, so `prx doctor` can judge the daemon
                // PATH against the directory the *daemon* runs from instead of
                // guessing from wherever the doctor process happens to live.
                if let Ok(binary) = std::env::current_exe() {
                    obj.insert("binary_path".into(), serde_json::json!(binary.display().to_string()));
                }
                match serde_json::to_value(shared_config.status()) {
                    Ok(status) => {
                        obj.insert("config_generation".into(), status);
                    }
                    Err(error) => {
                        tracing::warn!("Failed to serialize config generation status: {error}");
                    }
                }
            }
            let data = serde_json::to_vec_pretty(&json).unwrap_or_else(|_| b"{}".to_vec());
            if let Err(e) = tokio::fs::write(&path, &data).await {
                tracing::warn!("Failed to write daemon state file: {e}");
            }
            systemd_notify::watchdog();
        }
    })
}

const fn xin_runtime_enabled(_config: &Config) -> bool {
    true
}

/// Path of the advisory lock that guarantees a single live `prx daemon` per
/// runtime directory. It sits next to `daemon_state.json` so all daemon runtime
/// artefacts stay in one directory.
pub fn lock_file_path(config: &Config) -> PathBuf {
    config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("daemon.lock")
}

/// How recently `daemon_state.json` must have been written for the daemon that
/// owns a config directory to count as live. The daemon rewrites it every
/// `STATUS_FLUSH_SECONDS`; allowing four flushes tolerates a loaded host
/// without reporting a daemon that exited minutes ago.
const LIVE_DAEMON_STATE_MAX_AGE_SECONDS: u64 = STATUS_FLUSH_SECONDS * 4;

/// Whether a live `prx daemon` is currently running out of `config_dir`.
///
/// Two independent signals, either of which is sufficient: the advisory lock in
/// `daemon.lock` is still held by another process, or `daemon_state.json` was
/// refreshed within the last few status flushes. Purely advisory — callers use
/// this to warn, never to block, so every failure to read a signal is simply
/// "no evidence of a live daemon".
#[must_use]
pub fn live_daemon_uses_config_dir(config_dir: &Path) -> bool {
    daemon_lock_is_held(&config_dir.join("daemon.lock"))
        || daemon_state_file_is_fresh(&config_dir.join("daemon_state.json"))
}

fn daemon_lock_is_held(path: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    // Taking the lock proves nobody else holds it; release it immediately so
    // this probe never competes with a daemon that is starting concurrently.
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

fn daemon_state_file_is_fresh(path: &Path) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    // A stamp in the future (clock skew) counts as fresh rather than stale.
    std::time::SystemTime::now()
        .duration_since(modified)
        .map_or(true, |age| age.as_secs() <= LIVE_DAEMON_STATE_MAX_AGE_SECONDS)
}

/// Exclusive `flock` held for the entire lifetime of the daemon process.
///
/// The kernel drops the lock when the descriptor is closed, which happens on a
/// clean exit *and* when the process is killed, so a crashed daemon never
/// leaves a stale lock that would block the next start.
#[derive(Debug)]
struct DaemonInstanceLock {
    _file: std::fs::File,
}

/// Take the process-wide single-instance lock, creating the runtime directory
/// when it does not exist yet.
fn acquire_single_instance_lock(config: &Config) -> Result<DaemonInstanceLock> {
    let path = lock_file_path(config);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create daemon runtime directory {}", parent.display()))?;
        }
    }
    lock_daemon_instance_at(&path)
}

fn lock_daemon_instance_at(path: &Path) -> Result<DaemonInstanceLock> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("Failed to open daemon lock file {}", path.display()))?;

    // Non-blocking: a second daemon must fail loudly instead of queueing behind
    // the live one.
    if let Err(error) = fs2::FileExt::try_lock_exclusive(&file) {
        anyhow::bail!(busy_daemon_message(path, lock_holder_pid(path), &error));
    }

    // The lock is ours now; publish the owning pid so operators can identify the
    // holder from the outside. A failure here is not fatal for mutual exclusion,
    // it only degrades the diagnostics of the *next* start attempt.
    if let Err(error) = write_lock_owner_pid(&mut file, std::process::id()) {
        tracing::warn!("failed to record daemon lock owner pid in {}: {error}", path.display());
    }

    Ok(DaemonInstanceLock { _file: file })
}

fn write_lock_owner_pid(file: &mut std::fs::File, pid: u32) -> std::io::Result<()> {
    file.set_len(0)?;
    std::io::Seek::seek(file, std::io::SeekFrom::Start(0))?;
    std::io::Write::write_all(file, format!("{pid}\n").as_bytes())?;
    std::io::Write::flush(file)
}

/// Best-effort read of the pid recorded by the daemon that owns the lock.
fn lock_holder_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn busy_daemon_message(path: &Path, holder_pid: Option<u32>, error: &std::io::Error) -> String {
    let holder = holder_pid.map_or_else(
        || "another prx daemon".to_string(),
        |pid| format!("another prx daemon (pid {pid})"),
    );
    let inspect = holder_pid.map_or_else(
        || "systemctl --user status prx.service".to_string(),
        |pid| format!("ps -p {pid} -o pid,lstart,cmd"),
    );
    format!(
        "Refusing to start: {holder} already holds the single-instance lock {lock}. \
         One runtime directory runs exactly one daemon. Inspect the running one with `{inspect}`, \
         and restart it with `systemctl --user restart prx.service` instead of starting a second \
         process (flock: {error})",
        lock = path.display(),
    )
}

fn spawn_component_supervisor<F, Fut>(
    name: &'static str,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    mut run_component: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        loop {
            crate::health::mark_component_starting(name);
            match run_component().await {
                Ok(()) => {
                    crate::health::mark_component_error(name, "component exited unexpectedly");
                    tracing::warn!("Daemon component '{name}' exited unexpectedly");
                    // Clean exit — reset backoff since the component ran successfully
                    backoff = initial_backoff_secs.max(1);
                }
                Err(e) => {
                    // The whole chain: the outer context alone ("… failed")
                    // does not say why (D-067).
                    crate::health::mark_component_error(name, format!("{e:#}"));
                    tracing::error!("Daemon component '{name}' failed: {}", redacted_chain(&e));
                }
            }

            crate::health::bump_component_restart(name);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

type GenerationComponentFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type GenerationComponentRunner =
    Arc<dyn Fn(Arc<crate::config::ConfigGeneration>, CancellationToken) -> GenerationComponentFuture + Send + Sync>;
type GenerationComponentEnabled = Arc<dyn Fn(&Config) -> bool + Send + Sync>;
type GenerationComponentFieldFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Clone)]
struct GenerationSupervisorController {
    name: &'static str,
    prepares_for_field: GenerationComponentFieldFilter,
    commands: tokio::sync::mpsc::UnboundedSender<GenerationSupervisorCommand>,
}

enum GenerationSupervisorCommand {
    Prepare {
        generation: Arc<crate::config::ConfigGeneration>,
        response: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
    },
    Commit {
        generation_id: u64,
        response: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
    },
    Finalize {
        generation_id: u64,
    },
    Rollback {
        generation_id: u64,
        response: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
    },
}

struct DaemonConfigGenerationParticipant {
    controllers: Vec<GenerationSupervisorController>,
}

struct PreparedDaemonConfigGeneration {
    controllers: Vec<GenerationSupervisorController>,
    generation_id: u64,
}

impl crate::config::PreparedConfigGeneration for PreparedDaemonConfigGeneration {
    fn commit(&mut self) -> Result<()> {
        for controller in &self.controllers {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            controller
                .commands
                .send(GenerationSupervisorCommand::Commit {
                    generation_id: self.generation_id,
                    response: tx,
                })
                .map_err(|_| anyhow::anyhow!("{} supervisor is unavailable during commit", controller.name))?;
            match rx.recv_timeout(Duration::from_secs(45)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => anyhow::bail!("{} commit failed: {error}", controller.name),
                Err(error) => anyhow::bail!("{} commit acknowledgement failed: {error}", controller.name),
            }
        }
        Ok(())
    }

    fn rollback(&mut self) {
        for controller in self.controllers.iter().rev() {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            if controller
                .commands
                .send(GenerationSupervisorCommand::Rollback {
                    generation_id: self.generation_id,
                    response: tx,
                })
                .is_err()
            {
                tracing::error!(
                    component = controller.name,
                    generation = self.generation_id,
                    "failed to request config-generation rollback"
                );
                continue;
            }
            match rx.recv_timeout(Duration::from_secs(45)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(
                    component = controller.name,
                    generation = self.generation_id,
                    "config-generation rollback failed: {error}"
                ),
                Err(error) => tracing::error!(
                    component = controller.name,
                    generation = self.generation_id,
                    "config-generation rollback acknowledgement failed: {error}"
                ),
            }
        }
    }

    fn finalize(&mut self) {
        for controller in &self.controllers {
            if controller
                .commands
                .send(GenerationSupervisorCommand::Finalize {
                    generation_id: self.generation_id,
                })
                .is_err()
            {
                tracing::error!(
                    component = controller.name,
                    generation = self.generation_id,
                    "failed to finalize committed config generation"
                );
            }
        }
    }
}

impl crate::config::ConfigGenerationParticipant for DaemonConfigGenerationParticipant {
    fn name(&self) -> &'static str {
        "daemon_component_supervisors"
    }

    fn supports_controlled_restart_field(&self, field: &str) -> bool {
        crate::config::generation::is_controlled_restart(field)
            && self
                .controllers
                .iter()
                .any(|controller| (controller.prepares_for_field)(field))
    }

    fn supports_rebuild_field(&self, field: &str) -> bool {
        crate::config::generation::is_rebuild_and_swap(field)
            && self
                .controllers
                .iter()
                .any(|controller| (controller.prepares_for_field)(field))
    }

    fn prepares_for_field(&self, field: &str) -> bool {
        self.controllers
            .iter()
            .any(|controller| (controller.prepares_for_field)(field))
    }

    fn prepare(
        &self,
        generation: Arc<crate::config::ConfigGeneration>,
        changed_fields: &[String],
    ) -> Result<Box<dyn crate::config::PreparedConfigGeneration>> {
        let controllers = self
            .controllers
            .iter()
            .filter(|controller| {
                changed_fields
                    .iter()
                    .any(|field| (controller.prepares_for_field)(field))
            })
            .cloned()
            .collect::<Vec<_>>();
        if controllers.is_empty() {
            return Ok(Box::new(PreparedDaemonConfigGeneration {
                controllers: Vec::new(),
                generation_id: generation.id.0,
            }));
        }

        let mut prepared = Vec::new();
        for controller in &controllers {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            controller
                .commands
                .send(GenerationSupervisorCommand::Prepare {
                    generation: Arc::clone(&generation),
                    response: tx,
                })
                .map_err(|_| anyhow::anyhow!("{} supervisor is unavailable", controller.name))?;
            match rx.recv_timeout(Duration::from_secs(45)) {
                Ok(Ok(())) => prepared.push(controller.clone()),
                Ok(Err(error)) => {
                    rollback_prepared_supervisors(&prepared, generation.id.0);
                    anyhow::bail!("{} failed to prepare: {error}", controller.name);
                }
                Err(error) => {
                    rollback_prepared_supervisors(&prepared, generation.id.0);
                    anyhow::bail!("{} prepare acknowledgement failed: {error}", controller.name);
                }
            }
        }

        Ok(Box::new(PreparedDaemonConfigGeneration {
            controllers: prepared,
            generation_id: generation.id.0,
        }))
    }
}

fn rollback_prepared_supervisors(controllers: &[GenerationSupervisorController], generation_id: u64) {
    for controller in controllers.iter().rev() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = controller.commands.send(GenerationSupervisorCommand::Rollback {
            generation_id,
            response: tx,
        });
        let _ = rx.recv_timeout(Duration::from_secs(45));
    }
}

fn spawn_config_generation_supervisor(
    name: &'static str,
    owner: &'static str,
    freshness_ttl_seconds: u64,
    initial_generation: Arc<crate::config::ConfigGeneration>,
    prepares_for_field: GenerationComponentFieldFilter,
    enabled: GenerationComponentEnabled,
    runner: GenerationComponentRunner,
    root_shutdown: CancellationToken,
) -> (JoinHandle<()>, GenerationSupervisorController) {
    // Class A — unbounded is safe here because production is self-limiting, not
    // because nothing is watched.
    //
    // Every command except `Finalize` carries a `sync_channel(1)` reply handle
    // that the sender immediately blocks on (`recv_timeout(45s)`), so a given
    // caller can never have more than one command outstanding; `Finalize` is
    // fire-and-forget but is emitted exactly once per generation, right after a
    // `Commit` that was already acknowledged. The steady-state queue depth is
    // therefore the number of concurrent config-generation drivers — a handful,
    // not a function of agent concurrency or stream volume.
    //
    // Bounding it would also be actively wrong: all senders are synchronous
    // trait methods (`PreparedConfigGeneration::{commit,rollback,finalize}`)
    // with no `.await` available, so a full bounded channel could only be
    // handled by dropping a commit or a rollback — silently corrupting the
    // generation state machine.
    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
    let controller = GenerationSupervisorController {
        name,
        prepares_for_field,
        commands,
    };
    let handle = tokio::spawn(run_config_generation_supervisor(
        name,
        owner,
        freshness_ttl_seconds,
        initial_generation,
        enabled,
        runner,
        root_shutdown,
        receiver,
    ));
    (handle, controller)
}

#[allow(clippy::too_many_arguments)]
async fn run_config_generation_supervisor(
    name: &'static str,
    owner: &'static str,
    freshness_ttl_seconds: u64,
    initial_generation: Arc<crate::config::ConfigGeneration>,
    enabled: GenerationComponentEnabled,
    runner: GenerationComponentRunner,
    root_shutdown: CancellationToken,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<GenerationSupervisorCommand>,
) {
    let mut current_generation = initial_generation;
    let mut previous_generation: Option<Arc<crate::config::ConfigGeneration>> = None;
    let mut committed_generation: Option<u64> = None;
    // Set while the component is down because a start failed (initial start,
    // restart, or a restore of the previous generation); the tick retries the
    // current generation with a growing backoff (D-072 L-5).
    let mut retry: Option<ComponentRetry> = None;
    let (mut task, mut task_shutdown) = match start_generation_component(
        name,
        owner,
        freshness_ttl_seconds,
        Arc::clone(&current_generation),
        &enabled,
        &runner,
    )
    .await
    {
        Ok(state) => state,
        Err(error) => {
            crate::health::mark_component_error(name, format!("{error:#}"));
            tracing::error!(
                component = name,
                "initial generation failed: {}",
                redacted_chain(&error)
            );
            retry = Some(ComponentRetry::first());
            (None, None)
        }
    };

    loop {
        tokio::select! {
            () = root_shutdown.cancelled() => {
                stop_generation_component(name, &mut task, &mut task_shutdown).await;
                break;
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    stop_generation_component(name, &mut task, &mut task_shutdown).await;
                    break;
                };
                match command {
                    GenerationSupervisorCommand::Prepare { generation, response } => {
                        if generation.id <= current_generation.id || previous_generation.is_some() {
                            let _ = response.send(Err(format!(
                                "stale or overlapping generation prepare rejected: current={}, candidate={}",
                                current_generation.id.0,
                                generation.id.0
                            )));
                            continue;
                        }
                        let old = Arc::clone(&current_generation);
                        stop_generation_component(name, &mut task, &mut task_shutdown).await;
                        match start_generation_component(
                            name,
                            owner,
                            freshness_ttl_seconds,
                            Arc::clone(&generation),
                            &enabled,
                            &runner,
                        ).await {
                            Ok((next_task, next_shutdown)) => {
                                task = next_task;
                                task_shutdown = next_shutdown;
                                current_generation = generation;
                                previous_generation = Some(old);
                                committed_generation = None;
                                let _ = response.send(Ok(()));
                            }
                            Err(error) => {
                                let rollback = start_generation_component(
                                    name,
                                    owner,
                                    freshness_ttl_seconds,
                                    Arc::clone(&old),
                                    &enabled,
                                    &runner,
                                ).await;
                                match rollback {
                                    Ok((old_task, old_shutdown)) => {
                                        task = old_task;
                                        task_shutdown = old_shutdown;
                                        current_generation = old;
                                    }
                                    Err(rollback_error) => {
                                        current_generation = old;
                                        retry = Some(ComponentRetry::first());
                                        // The component is down until the next
                                        // generation: say so in its health (D-067).
                                        crate::health::mark_component_error(
                                            name,
                                            // What happened first, the restore's chain
                                            // last: a bounded summary keeps both ends,
                                            // and the restore's root cause is why the
                                            // component is down (D-072 L-1).
                                            format!(
                                                "the previous generation could not restart (after the candidate \
                                                 generation failed: {error:#}): {rollback_error:#}"
                                            ),
                                        );
                                        tracing::error!(
                                            component = name,
                                            "candidate failed and old generation could not restart: {}",
                                            redacted_chain(&rollback_error)
                                        );
                                    }
                                }
                                // The whole chain, so the refusal (and the reload
                                // warning built from it) says why the candidate failed.
                                let _ = response.send(Err(format!("{error:#}")));
                            }
                        }
                    }
                    GenerationSupervisorCommand::Commit { generation_id, response } => {
                        if current_generation.id.0 != generation_id || previous_generation.is_none() {
                            let _ = response.send(Err(format!(
                                "stale generation commit rejected: current={}, candidate={generation_id}",
                                current_generation.id.0
                            )));
                            continue;
                        }
                        committed_generation = Some(generation_id);
                        let _ = response.send(Ok(()));
                    }
                    GenerationSupervisorCommand::Finalize { generation_id } => {
                        if current_generation.id.0 == generation_id
                            && committed_generation == Some(generation_id)
                        {
                            previous_generation = None;
                            committed_generation = None;
                        } else {
                            tracing::warn!(
                                component = name,
                                current_generation = current_generation.id.0,
                                generation = generation_id,
                                "stale config-generation finalize rejected"
                            );
                        }
                    }
                    GenerationSupervisorCommand::Rollback { generation_id, response } => {
                        if current_generation.id.0 != generation_id {
                            let _ = response.send(Err(format!(
                                "stale generation rollback rejected: current={}, candidate={generation_id}",
                                current_generation.id.0
                            )));
                            continue;
                        }
                        let Some(previous) = previous_generation.take() else {
                            let _ = response.send(Err("previous generation is unavailable".to_string()));
                            continue;
                        };
                        stop_generation_component(name, &mut task, &mut task_shutdown).await;
                        match start_generation_component(
                            name,
                            owner,
                            freshness_ttl_seconds,
                            Arc::clone(&previous),
                            &enabled,
                            &runner,
                        ).await {
                            Ok((old_task, old_shutdown)) => {
                                task = old_task;
                                task_shutdown = old_shutdown;
                                current_generation = previous;
                                committed_generation = None;
                                let _ = response.send(Ok(()));
                            }
                            Err(error) => {
                                // The previous generation is the one to run; the
                                // tick keeps trying to bring it back.
                                current_generation = previous;
                                committed_generation = None;
                                retry = Some(ComponentRetry::first());
                                // The previous generation could not be restored:
                                // the component is down, so its health says why,
                                // and the reply carries the whole chain (D-067).
                                crate::health::mark_component_error(
                                    name,
                                    format!("rollback to the previous generation failed: {error:#}"),
                                );
                                tracing::error!(
                                    component = name,
                                    "rollback to the previous generation failed: {}",
                                    redacted_chain(&error)
                                );
                                let _ = response.send(Err(format!("{error:#}")));
                            }
                        }
                    }
                }
            }
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                if task.is_none() && retry.as_ref().is_some_and(ComponentRetry::is_due) {
                    crate::health::bump_component_restart(name);
                    match start_generation_component(
                        name,
                        owner,
                        freshness_ttl_seconds,
                        Arc::clone(&current_generation),
                        &enabled,
                        &runner,
                    ).await {
                        Ok((next_task, next_shutdown)) => {
                            task = next_task;
                            task_shutdown = next_shutdown;
                            retry = None;
                        }
                        Err(error) => {
                            crate::health::mark_component_error(name, format!("{error:#}"));
                            tracing::error!(component = name, "component retry failed: {}", redacted_chain(&error));
                            retry = retry.map(ComponentRetry::next);
                        }
                    }
                } else if task.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
                    if let Some(finished) = task.take() {
                        match finished.await {
                            Ok(Ok(())) => tracing::warn!(component = name, "component exited unexpectedly"),
                            Ok(Err(error)) => tracing::error!(component = name, "component failed: {}", redacted_chain(&error)),
                            Err(error) => tracing::error!(component = name, "component task failed: {error}"),
                        }
                    }
                    task_shutdown = None;
                    crate::health::bump_component_restart(name);
                    match start_generation_component(
                        name,
                        owner,
                        freshness_ttl_seconds,
                        Arc::clone(&current_generation),
                        &enabled,
                        &runner,
                    ).await {
                        Ok((next_task, next_shutdown)) => {
                            task = next_task;
                            task_shutdown = next_shutdown;
                        }
                        Err(error) => {
                            crate::health::mark_component_error(name, format!("{error:#}"));
                            tracing::error!(component = name, "component restart failed: {}", redacted_chain(&error));
                            retry = Some(ComponentRetry::first());
                        }
                    }
                }
            }
        }
    }
}

/// When a component that failed to start is tried again.
struct ComponentRetry {
    at: tokio::time::Instant,
    backoff: Duration,
}

impl ComponentRetry {
    const FIRST: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_mins(1);

    fn first() -> Self {
        Self {
            at: tokio::time::Instant::now() + Self::FIRST,
            backoff: Self::FIRST,
        }
    }

    fn next(self) -> Self {
        let backoff = (self.backoff * 2).min(Self::MAX);
        Self {
            at: tokio::time::Instant::now() + backoff,
            backoff,
        }
    }

    fn is_due(&self) -> bool {
        tokio::time::Instant::now() >= self.at
    }
}

async fn start_generation_component(
    name: &'static str,
    owner: &'static str,
    freshness_ttl_seconds: u64,
    generation: Arc<crate::config::ConfigGeneration>,
    enabled: &GenerationComponentEnabled,
    runner: &GenerationComponentRunner,
) -> Result<(Option<JoinHandle<Result<()>>>, Option<CancellationToken>)> {
    let should_run = enabled(&generation.effective);
    register_optional_component(name, owner, should_run, freshness_ttl_seconds);
    if !should_run {
        return Ok((None, None));
    }
    let token = CancellationToken::new();
    let future = runner(Arc::clone(&generation), token.clone());
    let mut handle = tokio::spawn(future);
    let readiness = async {
        loop {
            tokio::select! {
                result = &mut handle => {
                    return match result {
                        Ok(Ok(())) => Err(anyhow::anyhow!("{name} exited during generation preparation")),
                        Ok(Err(error)) => Err(error).with_context(|| format!("{name} failed during generation preparation")),
                        Err(error) => Err(anyhow::anyhow!("{name} task failed during generation preparation: {error}")),
                    };
                }
                () = tokio::time::sleep(Duration::from_millis(25)) => {
                    match component_health_status(name).as_deref() {
                        Some("ok") => return Ok(()),
                        Some("error") => {
                            // Keep the component's own cause: the supervisor
                            // overwrites its health with this error (D-072 L-2).
                            let cause = crate::health::snapshot()
                                .components
                                .get(name)
                                .and_then(|component| component.last_error.clone());
                            return Err(cause.map_or_else(
                                || anyhow::anyhow!("{name} reported failed health during generation preparation"),
                                |cause| anyhow::anyhow!(
                                    "{name} reported failed health during generation preparation: {cause}"
                                ),
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
    };
    match tokio::time::timeout(generation_readiness_timeout(), readiness).await {
        Ok(Ok(())) => Ok((Some(handle), Some(token))),
        Ok(Err(error)) => {
            token.cancel();
            // D-065 R-1: when the component ended during preparation the
            // readiness loop already took its result; awaiting the handle
            // again panics ("JoinHandle polled after completion") and kills
            // this supervisor, so a refused candidate (for example a channel
            // runtime whose default cannot chat) left the component stopped
            // instead of restarting the previous generation.
            if !handle.is_finished() {
                handle.abort();
                let _ = handle.await;
            }
            Err(error)
        }
        Err(_) => {
            token.cancel();
            handle.abort();
            let _ = handle.await;
            anyhow::bail!(
                "{name} did not acknowledge readiness within {} milliseconds",
                generation_readiness_timeout().as_millis()
            )
        }
    }
}

#[cfg(not(test))]
const fn generation_readiness_timeout() -> Duration {
    Duration::from_secs(45)
}

#[cfg(test)]
const fn generation_readiness_timeout() -> Duration {
    Duration::from_millis(500)
}

/// An error's whole chain for a log line, with secrets redacted: inner
/// errors (a request URL, a token in a path) can carry them, and the health
/// copy of the same text is sanitized too (D-072 L-3).
fn redacted_chain(error: &anyhow::Error) -> String {
    crate::security::redaction::redact_secrets(&format!("{error:#}"))
}

fn component_health_status(name: &str) -> Option<String> {
    crate::health::snapshot_json()
        .get("components")
        .and_then(|components| components.get(name))
        .and_then(|component| component.get("status"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

async fn stop_generation_component(
    name: &'static str,
    task: &mut Option<JoinHandle<Result<()>>>,
    shutdown: &mut Option<CancellationToken>,
) {
    if let Some(token) = shutdown.take() {
        token.cancel();
    }
    let Some(mut handle) = task.take() else {
        return;
    };
    match tokio::time::timeout(Duration::from_secs(30), &mut handle).await {
        Ok(_) => {}
        Err(_) => {
            tracing::warn!(component = name, "component generation drain timed out; aborting");
            handle.abort();
            let _ = handle.await;
        }
    }
}

async fn run_fitness_worker(
    config: Config,
    manager: crate::config::SharedConfig,
    generation_id: crate::config::ConfigGenerationId,
) -> Result<()> {
    crate::self_system::fitness::validate_fitness_config(&config.self_system.fitness)?;
    let mut health_heartbeat = tokio::time::interval(Duration::from_mins(1));
    crate::health::mark_component_ok("self_system_fitness");
    wait_for_active_generation(&manager, generation_id).await;

    // Catch up the latest closed day exactly once. Unlike `tokio::interval`,
    // this does not manufacture a partial-day report immediately on startup.
    run_fitness_if_due(&config).await;

    loop {
        let next_run =
            crate::self_system::fitness::next_scheduled_run(chrono::Utc::now(), &config.self_system.fitness)?;
        let wait = (next_run - chrono::Utc::now())
            .to_std()
            .unwrap_or_else(|_| Duration::from_secs(1));
        tokio::select! {
            _ = health_heartbeat.tick() => crate::health::touch_component("self_system_fitness"),
            () = tokio::time::sleep(wait) => run_fitness_if_due(&config).await,
        }
    }
}

async fn run_fitness_if_due(config: &Config) {
    let latest =
        match crate::self_system::fitness::latest_closed_window(chrono::Utc::now(), &config.self_system.fitness) {
            Ok(window) => window,
            Err(error) => {
                crate::health::mark_component_error("self_system_fitness", format!("{error:#}"));
                tracing::warn!(target: "self_system", "fitness schedule failed: {}", redacted_chain(&error));
                return;
            }
        };
    run_fitness_backfill(config, latest, |window| run_fitness_window_if_missing(config, window)).await;
}

async fn run_fitness_backfill<F, Fut>(
    config: &Config,
    latest: crate::self_system::fitness::WindowBounds,
    mut run_window: F,
) where
    F: FnMut(crate::self_system::fitness::WindowBounds) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let backfill_days = config.self_system.fitness.max_backfill_days.max(1);
    for offset in (0..backfill_days).rev() {
        let Some(day) = latest.day.checked_sub_days(chrono::Days::new(u64::from(offset))) else {
            continue;
        };
        let window = match crate::self_system::fitness::configured_window_for_day(day, &config.self_system.fitness) {
            Ok(window) => window,
            Err(error) => {
                crate::health::mark_component_error("self_system_fitness", format!("{error:#}"));
                tracing::warn!(target: "self_system", "fitness backfill window failed: {}", redacted_chain(&error));
                return;
            }
        };
        if !run_window(window).await {
            return;
        }
    }
}

async fn run_fitness_window_if_missing(config: &Config, window: crate::self_system::fitness::WindowBounds) -> bool {
    let store = crate::self_system::fitness_store::FitnessStore::new(&config.workspace_dir);
    match store.load(window.day) {
        Ok(Some(_)) => {
            crate::health::mark_component_ok("self_system_fitness");
            return true;
        }
        Ok(None) => {}
        Err(error) => {
            crate::health::mark_component_error("self_system_fitness", format!("{error:#}"));
            tracing::warn!(target: "self_system", "fitness store read failed: {}", redacted_chain(&error));
            return false;
        }
    }
    match crate::self_system::fitness::run_fitness_report_for_window(config, window).await {
        Ok(report) => {
            crate::health::mark_component_ok("self_system_fitness");
            tracing::info!(
                target: "self_system",
                score = report.final_score,
                confidence = report.confidence,
                coverage = report.coverage,
                status = ?report.status,
                date = report.window.date,
                "fitness report stored"
            );
            true
        }
        Err(error) => {
            crate::health::mark_component_error("self_system_fitness", format!("{error:#}"));
            tracing::warn!(target: "self_system", "fitness report failed: {}", redacted_chain(&error));
            false
        }
    }
}

async fn run_evolution_scheduler_worker(
    config: Config,
    manager: crate::config::SharedConfig,
    generation_id: crate::config::ConfigGenerationId,
) -> Result<()> {
    let (mut scheduler, interval_hours) = build_evolution_scheduler(&config).await?;
    let mut interval = tokio::time::interval(Duration::from_secs(interval_hours.max(1).saturating_mul(3600)));
    let mut health_heartbeat = tokio::time::interval(Duration::from_mins(1));
    crate::health::mark_component_ok("evolution_scheduler");
    wait_for_active_generation(&manager, generation_id).await;

    loop {
        tokio::select! {
            _ = health_heartbeat.tick() => {
                // Preserve failed states while refreshing the liveness of the
                // long-lived scheduler and judge owners.
                crate::health::touch_component("evolution_scheduler");
                crate::health::touch_component(EVOLUTION_JUDGE_COMPONENT);
            }
            _ = interval.tick() => {
                match crate::skills::lifecycle_runtime::run_experience_retention(
                    &config,
                    scheduler.experience_retention(),
                ).await {
                    Ok(removed) => tracing::info!(removed, "skill experience retention finished"),
                    Err(error) => tracing::warn!(%error, "skill experience retention deferred"),
                }
                match scheduler.run_scheduled(Utc::now()).await {
                    Ok(summary) => {
                        if summary.digest_ran || summary.cycle_ran {
                            let gate_denied = summary.layer_reports.iter().filter(|report| report.gate_denied).count();
                            tracing::info!(
                                target: "self_system",
                                digest_ran = summary.digest_ran,
                                cycle_ran = summary.cycle_ran,
                                layer_reports = summary.layer_reports.len(),
                                gate_denied,
                                "evolution scheduler tick completed"
                            );
                        }
                        record_evolution_judge_health(
                            EVOLUTION_JUDGE_COMPONENT,
                            summary
                                .layer_reports
                                .iter()
                                .map(|report| (report.judge_outcome, report.errors.as_slice())),
                        );
                        crate::health::mark_component_ok("evolution_scheduler");
                    }
                    Err(error) => {
                        crate::health::mark_component_error("evolution_scheduler", format!("{error:#}"));
                        tracing::warn!(target: "self_system", "evolution scheduler tick failed: {}", redacted_chain(&error));
                    }
                }
            }
        }
    }
}

async fn wait_for_active_generation(
    manager: &crate::config::SharedConfig,
    generation_id: crate::config::ConfigGenerationId,
) {
    while manager.active_generation_id() != generation_id {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn build_evolution_scheduler(config: &Config) -> Result<(EvolutionScheduler, u64)> {
    let (cfg, cfg_path) = load_evolution_config(config).await?;
    tokio::fs::create_dir_all(&config.workspace_dir)
        .await
        .with_context(|| {
            format!(
                "failed to prepare workspace dir for evolution scheduler: {}",
                config.workspace_dir.display()
            )
        })?;

    #[cfg(feature = "wasm-plugins")]
    let wasm_early_runtime = if config
        .default_provider
        .as_deref()
        .is_some_and(|provider| provider.starts_with("wasm:"))
        || crate::memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config))
            .starts_with("wasm:")
    {
        crate::plugins::init_plugin_runtime(config, None).await
    } else {
        None
    };

    let shared = new_shared_evolution_config(cfg.clone());
    let storage_root = resolve_evolution_storage_root(config, &cfg.runtime);
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            retention_from_runtime(&cfg.runtime.retention),
            cfg.runtime.batch_size,
        )
        .await?,
    );
    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), storage_root.join("analysis")));

    // FIX-P1-10: the production scheduler judges evolution cycles with a real
    // LLM-backed `ManagedJudge` (falling back to the deterministic mock only when
    // no provider can be initialized) instead of always using `MockJudgeModel`.
    let judge_model = build_evolution_judge_model(config);
    // FIX-P0-40: the autonomous evolution pipeline must pass every commit through
    // the same `SideEffectGate` as tool execution. Build the runtime security
    // policy from config (honouring `security.audit`, FIX-P1-31) and install it on
    // the pipeline so an evolution self-modification is gated by autonomy.
    let security_policy = crate::runtime::bootstrap::build_security_policy(config);
    let pipeline = EvolutionPipeline::with_judge_model(
        shared.clone(),
        analyzer.clone(),
        writer.clone(),
        &config.workspace_dir,
        judge_model,
    )
    .with_security_policy(security_policy)
    .with_judge_timeout(Duration::from_secs(config.self_system.evolution_judge_timeout_secs))
    .with_commit_tracker(crate::self_system::evolution::commit::daemon_commits().clone());

    let evolution_memory: Arc<dyn crate::memory::Memory> =
        Arc::from(crate::memory::create_memory_with_storage_and_routes_with_acl(
            &config.memory,
            &config.embedding_routes_with_references(),
            Some(&config.storage.provider.config),
            &config.workspace_dir,
            crate::memory::EmbeddingRootCredential::from_config(&config),
            &config.identity_bindings,
            &config.user_policies,
        )?);
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = wasm_early_runtime {
        runtime
            .attach_memory(Arc::clone(&evolution_memory))
            .await
            .map_err(|error| anyhow::anyhow!("failed to attach WASM memory backend for evolution: {error}"))?;
    }
    let memory_engine = Box::new(
        MemoryEvolutionEngine::new(shared.clone(), &cfg_path)
            .with_context(|| format!("failed to initialize memory evolution engine: {}", cfg_path.display()))?
            .with_memory(evolution_memory),
    );
    let mut prompt_engine = PromptEvolutionEngine::new(shared.clone(), &config.workspace_dir, Some(writer.clone()))
        .with_debug_raw(config.self_system.evolution_debug_raw)
        .with_lifecycle(crate::skills::lifecycle_runtime::open_service(
            &config.workspace_dir,
            &config.skills_lifecycle,
        )?);
    match crate::runtime::evolution_model::EvolutionModel::from_config(
        config,
        crate::runtime::evolution_model::EvolutionPurpose::Proposer,
    ) {
        Ok(model) => {
            prompt_engine = prompt_engine.with_proposer(model);
        }
        Err(_) => {
            tracing::warn!(target: "self_system", "managed evolution proposer unavailable; prompt generation will be refused")
        }
    }
    let prompt_engine = Box::new(prompt_engine);
    let strategy_engine = Box::new(StrategyEvolutionEngine::new(
        shared.clone(),
        &config.workspace_dir,
        writer,
    )?);

    let scheduler = EvolutionScheduler::new(
        shared,
        analyzer,
        pipeline,
        config.workspace_dir.join(".evolution/scheduler_state.json"),
        memory_engine,
        prompt_engine,
        strategy_engine,
    );

    Ok((scheduler, u64::from(config.self_system.evolution_interval_hours.max(1))))
}

/// How long `daemon::run` waits for evolution verdict tasks on shutdown.
const EVOLUTION_COMMIT_DRAIN: Duration = Duration::from_secs(10);

/// Cancel the evolution verdict tasks on `commits` and wait for them, up to
/// [`EVOLUTION_COMMIT_DRAIN`]: a cancelled task writes nothing it has not
/// already been accepted to write, and records its cycle (D-046).
pub(crate) async fn drain_evolution_commits(commits: &crate::self_system::evolution::commit::CommitTracker) {
    let pending = commits.active();
    if !commits.drain(EVOLUTION_COMMIT_DRAIN).await {
        tracing::warn!(
            target: "self_system",
            pending,
            still_running = commits.active(),
            "evolution verdict tasks did not finish within the shutdown bound"
        );
    }
}

/// Health component name reporting whether evolution judging runs on a real
/// LLM judge or has degraded to the deterministic mock judge (FIX-P1-10).
const EVOLUTION_JUDGE_COMPONENT: &str = "evolution_judge";

/// R-PMMCh S-1: reflect one scheduler tick's verdicts on the judge health
/// component. A judge that gave no verdict (error, unparseable reply,
/// timeout) marks it as an error naming why; a scored verdict marks it ok
/// again. Mock (degraded) verdicts leave the status that start-up recorded.
fn record_evolution_judge_health<'a>(
    component: &str,
    verdicts: impl IntoIterator<Item = (crate::self_system::evolution::pipeline::JudgeOutcome, &'a [String])>,
) {
    use crate::self_system::evolution::pipeline::JudgeOutcome;

    let mut scored = false;
    for (outcome, errors) in verdicts {
        match outcome {
            JudgeOutcome::Failed => {
                let reason = errors
                    .iter()
                    .find(|error| error.starts_with("judge_failed"))
                    .map_or("the evolution judge gave no verdict", String::as_str);
                // The judge error comes from the provider stack; the endpoint
                // is named without URL userinfo.
                crate::health::mark_component_error(
                    component,
                    crate::providers::redact_endpoint_userinfo(reason).into_owned(),
                );
                return;
            }
            JudgeOutcome::Scored => scored = true,
            JudgeOutcome::NotRun | JudgeOutcome::Degraded => {}
        }
    }
    if scored {
        crate::health::mark_component_ok(component);
    }
}

/// FIX-P1-10: build the scoring model used to judge evolution cycles.
///
/// The production path prefers a real LLM-backed [`crate::self_system::evolution::judge::ManagedJudge`] driven by the
/// configured default provider. Constructing a provider can fail (missing
/// credentials, unavailable backend); in that case we fall back to the
/// deterministic [`MockJudgeModel`] so a credential gap degrades judging quality
/// rather than disabling the evolution scheduler entirely.
///
/// The degradation is made observable rather than silent: it is logged at WARN
/// with an explicit "DEGRADED mock mode" message and recorded on the
/// [`EVOLUTION_JUDGE_COMPONENT`] health component (error when degraded, ok when a
/// real judge is wired) so the daemon health surface reflects it.
fn build_evolution_judge_model(config: &Config) -> Arc<dyn crate::self_system::evolution::judge::JudgeScoringModel> {
    use crate::self_system::evolution::judge::{ManagedJudge, MockJudgeModel};
    let provider_name = crate::providers::selection::default_provider(config).to_string();
    let built = crate::runtime::evolution_model::EvolutionModel::from_config(
        config,
        crate::runtime::evolution_model::EvolutionPurpose::Judge,
    );
    match built {
        Ok(model) => {
            crate::health::mark_component_ok(EVOLUTION_JUDGE_COMPONENT);
            Arc::new(ManagedJudge(model))
        }
        Err(error) => {
            // The default provider may be a `custom:` URL carrying
            // `user:password@`; the health record and the log name the
            // endpoint without it.
            let provider_name = crate::providers::redact_endpoint_userinfo(&provider_name);
            let error = crate::providers::redact_endpoint_userinfo(&error.to_string()).into_owned();
            crate::health::mark_component_error(
                EVOLUTION_JUDGE_COMPONENT,
                format!("provider '{provider_name}' unavailable: {error}"),
            );
            tracing::warn!(
                target: "self_system",
                provider = %provider_name,
                error = %error,
                judge_degraded = true,
                "evolution judging running in DEGRADED mock mode: provider unavailable, \
                 quality scores are deterministic placeholders, not real LLM judgments"
            );
            Arc::new(MockJudgeModel)
        }
    }
}

async fn load_evolution_config(config: &Config) -> Result<(EvolutionConfig, PathBuf)> {
    let path = discover_evolution_config_path(config);
    if tokio::fs::metadata(&path).await.is_ok() {
        let cfg = EvolutionConfig::load_from_path(&path)
            .await
            .with_context(|| format!("failed to load evolution config: {}", path.display()))?;
        Ok((cfg, path))
    } else {
        Ok((EvolutionConfig::default(), path))
    }
}

fn discover_evolution_config_path(config: &Config) -> PathBuf {
    if let Some(raw) = config.self_system.evolution_config_path.as_deref() {
        let path = PathBuf::from(raw);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }

    let candidates = [
        config.workspace_dir.join("evolution_config.toml"),
        PathBuf::from("evolution_config.toml"),
        PathBuf::from("config/evolution_config.toml"),
    ];

    for candidate in &candidates {
        // NOTE: TOCTOU safe — read-only config discovery. If the file disappears
        // between this check and the subsequent read, `EvolutionConfig::load_from_path`
        // will return an error that is handled by the caller. No security impact.
        if candidate.exists() {
            return candidate.clone();
        }
    }
    candidates[0].clone()
}

fn resolve_evolution_storage_root(config: &Config, runtime: &EvolutionRuntimeConfig) -> PathBuf {
    let root = Path::new(&runtime.storage_dir);
    if root.is_absolute() {
        root.to_path_buf()
    } else {
        config.workspace_dir.join(root)
    }
}

const fn retention_from_runtime(retention: &EvolutionRetentionConfig) -> JsonlRetentionPolicy {
    JsonlRetentionPolicy {
        hot_days: retention.hot_days,
        warm_days: retention.warm_days,
        cold_days: retention.cold_days,
    }
}

/// The fields whose change makes the channels supervisor prepare a new
/// channel runtime for the candidate generation: its own table and every
/// rebuild-and-swap field, which includes the default provider and model,
/// the provider tables and the model routes. The new runtime resolves its
/// default before anything is built (`check_channel_startup_default`), so a
/// candidate whose default cannot chat fails the prepare and the generation
/// is refused (D-065 R-1).
fn channels_prepares_for_field(field: &str) -> bool {
    field == "channels_config" || crate::config::generation::is_rebuild_and_swap(field)
}

#[cfg_attr(not(feature = "wasm-plugins"), allow(clippy::missing_const_for_fn))]
fn has_supervised_channels(config: &Config) -> bool {
    let crate::config::ChannelsConfig {
        cli: _,     // `cli` is used only when running the CLI manually
        webhook: _, // Managed by the gateway
        telegram,
        discord,
        slack,
        mattermost,
        imessage,
        matrix,
        signal,
        whatsapp,
        email,
        irc,
        lark,
        dingtalk,
        linq,
        nextcloud_talk,
        qq,
        wacli,
        ..
    } = &config.channels_config;

    #[cfg(feature = "wasm-plugins")]
    let plugin_channel = config.plugins.enabled
        && std::fs::read_dir(config.workspace_dir.join("plugins")).is_ok_and(|entries| {
            entries.filter_map(|entry| entry.ok()).any(|entry| {
                if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    return false;
                }
                crate::plugins::manifest::PluginManifest::from_file(&entry.path().join("plugin.toml"))
                    .is_ok_and(|manifest| manifest.has_capability("channel"))
            })
        });
    #[cfg(not(feature = "wasm-plugins"))]
    let plugin_channel = false;
    plugin_channel
        || telegram.is_some()
        || discord.is_some()
        || slack.is_some()
        || mattermost.is_some()
        || imessage.is_some()
        || matrix.is_some()
        || signal.is_some()
        || whatsapp.is_some()
        || email.is_some()
        || irc.is_some()
        || lark.is_some()
        || dingtalk.is_some()
        || linq.is_some()
        || nextcloud_talk.is_some()
        || qq.is_some()
        || wacli.is_some()
}

mod systemd_notify {
    pub fn ready() {
        notify("READY=1\nSTATUS=PRX daemon ready");
    }

    pub fn watchdog() {
        notify("WATCHDOG=1");
    }

    pub fn stopping() {
        notify("STOPPING=1\nSTATUS=PRX daemon stopping");
    }

    fn notify(payload: &str) {
        match send(payload) {
            Ok(true) => tracing::trace!("Sent systemd notify payload"),
            Ok(false) => {}
            Err(error) => tracing::debug!("Failed to send systemd notify payload: {error}"),
        }
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[allow(unsafe_code)]
    fn send(payload: &str) -> std::io::Result<bool> {
        let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
            return Ok(false);
        };
        let socket = socket.to_string_lossy();
        if socket.is_empty() {
            return Ok(false);
        }

        // Linux abstract namespace sockets are encoded by systemd as '@name'.
        // libc is used because std UnixDatagram only accepts filesystem paths.
        // SAFETY: send_linux_datagram validates NOTIFY_SOCKET length and builds a bounded sockaddr_un.
        unsafe { send_linux_datagram(&socket, payload.as_bytes()) }?;
        Ok(true)
    }

    #[cfg(not(all(unix, target_os = "linux")))]
    const fn send(_payload: &str) -> std::io::Result<bool> {
        Ok(false)
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[allow(unsafe_code)]
    unsafe fn send_linux_datagram(socket: &str, payload: &[u8]) -> std::io::Result<()> {
        use std::ffi::CString;
        use std::mem::{MaybeUninit, size_of};
        use std::os::fd::RawFd;

        struct Fd(RawFd);
        impl Drop for Fd {
            fn drop(&mut self) {
                // SAFETY: this type owns the file descriptor.
                unsafe {
                    libc::close(self.0);
                }
            }
        }

        // SAFETY: constants form a valid AF_UNIX datagram socket request.
        let raw_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if raw_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = Fd(raw_fd);

        let addr = MaybeUninit::<libc::sockaddr_un>::zeroed();
        // SAFETY: zeroed sockaddr_un is initialized below before sendto.
        let mut addr = unsafe { addr.assume_init() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

        let name_len = if let Some(abstract_name) = socket.strip_prefix('@') {
            let bytes = abstract_name.as_bytes();
            if bytes.len() + 1 > addr.sun_path.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "NOTIFY_SOCKET abstract path is too long",
                ));
            }
            addr.sun_path[0] = 0;
            for (idx, byte) in bytes.iter().enumerate() {
                if let Some(slot) = addr.sun_path.get_mut(idx + 1) {
                    *slot = *byte as libc::c_char;
                }
            }
            bytes.len() + 1
        } else {
            let c_socket = CString::new(socket).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "NOTIFY_SOCKET contains interior NUL")
            })?;
            let bytes = c_socket.as_bytes_with_nul();
            if bytes.len() > addr.sun_path.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "NOTIFY_SOCKET path is too long",
                ));
            }
            for (idx, byte) in bytes.iter().enumerate() {
                if let Some(slot) = addr.sun_path.get_mut(idx) {
                    *slot = *byte as libc::c_char;
                }
            }
            bytes.len()
        };

        let addr_len = (size_of::<libc::sa_family_t>() + name_len) as libc::socklen_t;
        // SAFETY: fd is valid; addr points to initialized sockaddr_un bytes of addr_len.
        let sent = unsafe {
            libc::sendto(
                fd.0,
                payload.as_ptr().cast(),
                payload.len(),
                libc::MSG_NOSIGNAL,
                (&raw const addr).cast(),
                addr_len,
            )
        };
        if sent < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    #[test]
    fn state_file_path_uses_config_directory() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let path = state_file_path(&config);
        assert_eq!(path, tmp.path().join("daemon_state.json"));
    }

    #[test]
    fn xin_runtime_is_always_enabled() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        assert!(xin_runtime_enabled(&config));
    }

    #[test]
    fn lock_file_path_uses_config_directory() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        assert_eq!(lock_file_path(&config), tmp.path().join("daemon.lock"));
    }

    #[test]
    fn single_instance_lock_creates_missing_runtime_directory() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.config_path = tmp.path().join("nested").join("deeper").join("config.toml");

        let guard = acquire_single_instance_lock(&config).unwrap();
        assert!(lock_file_path(&config).exists());
        drop(guard);
    }

    #[test]
    fn single_instance_lock_records_owner_pid() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let guard = acquire_single_instance_lock(&config).unwrap();
        assert_eq!(lock_holder_pid(&lock_file_path(&config)), Some(std::process::id()));
        drop(guard);
    }

    #[test]
    fn second_lock_attempt_in_same_process_is_rejected() {
        // flock is per open file description, so two independent `open` calls
        // contend even inside one process.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.lock");

        let first = lock_daemon_instance_at(&path).unwrap();
        let error = lock_daemon_instance_at(&path).unwrap_err().to_string();
        assert!(error.contains("single-instance lock"), "unexpected message: {error}");
        assert!(
            error.contains(&std::process::id().to_string()),
            "message should name the holder pid: {error}"
        );
        drop(first);
    }

    #[test]
    fn lock_is_released_when_the_guard_is_dropped() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.lock");

        drop(lock_daemon_instance_at(&path).unwrap());
        assert!(lock_daemon_instance_at(&path).is_ok());
    }

    #[test]
    fn busy_message_without_recorded_pid_still_explains_the_conflict() {
        let error = std::io::Error::from(std::io::ErrorKind::WouldBlock);
        let message = busy_daemon_message(Path::new("/run/prx/daemon.lock"), None, &error);
        assert!(message.contains("/run/prx/daemon.lock"), "{message}");
        assert!(message.contains("prx.service"), "{message}");
    }

    #[test]
    fn lock_holder_pid_ignores_garbage_content() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.lock");
        std::fs::write(&path, b"not-a-pid").unwrap();

        assert_eq!(lock_holder_pid(&path), None);
    }

    #[tokio::test]
    async fn fitness_daemon_backfills_closed_days_oldest_first_and_skips_existing_reports() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.self_system.fitness.timezone = "UTC".to_string();
        config.self_system.fitness.max_backfill_days = 3;
        config.self_system.fitness.min_samples = 1;

        run_fitness_if_due(&config).await;

        let store = crate::self_system::fitness_store::FitnessStore::new(&config.workspace_dir);
        let latest_window =
            crate::self_system::fitness::latest_closed_window(Utc::now(), &config.self_system.fitness).unwrap();
        let reports = store.history(10).unwrap();
        assert_eq!(reports.len(), 3, "fixture must cross the max_backfill_days boundary");
        assert_eq!(reports[0].window.date, latest_window.day.to_string());
        assert_eq!(
            reports[2].window.date,
            (latest_window.day - chrono::Duration::days(2)).to_string()
        );
        assert_eq!(
            store.latest().unwrap().unwrap().window.date,
            latest_window.day.to_string()
        );
        assert!(
            store.load(Utc::now().date_naive()).unwrap().is_none(),
            "startup catch-up must not create a partial current-day report"
        );
        let generated_at = reports
            .iter()
            .map(|report| report.generated_at.clone())
            .collect::<Vec<_>>();

        run_fitness_if_due(&config).await;

        let rerun = store.history(10).unwrap();
        assert_eq!(rerun.len(), 3);
        assert_eq!(
            rerun
                .iter()
                .map(|report| report.generated_at.clone())
                .collect::<Vec<_>>(),
            generated_at,
            "existing reports must be skipped rather than regenerated"
        );
    }

    #[tokio::test]
    async fn fitness_daemon_stops_backfill_after_the_first_failed_day() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.self_system.fitness.timezone = "UTC".to_string();
        config.self_system.fitness.max_backfill_days = 4;
        let latest =
            crate::self_system::fitness::latest_closed_window(Utc::now(), &config.self_system.fitness).unwrap();
        let failed_day = latest.day - chrono::Duration::days(2);
        let attempted = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

        run_fitness_backfill(&config, latest, {
            let attempted = std::sync::Arc::clone(&attempted);
            move |window| {
                let attempted = std::sync::Arc::clone(&attempted);
                async move {
                    attempted.lock().push(window.day);
                    window.day != failed_day
                }
            }
        })
        .await;

        assert_eq!(
            *attempted.lock(),
            vec![latest.day - chrono::Duration::days(3), failed_day],
            "newer days must not run after the first failed backfill"
        );
    }

    #[test]
    fn resolve_evolution_storage_root_uses_workspace_for_relative_path() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let mut runtime = EvolutionRuntimeConfig::default();
        runtime.storage_dir = "self/evolution-data".to_string();

        let root = resolve_evolution_storage_root(&config, &runtime);
        assert_eq!(root, config.workspace_dir.join("self/evolution-data"));
    }

    #[test]
    fn resolve_evolution_storage_root_preserves_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let mut runtime = EvolutionRuntimeConfig::default();
        let abs = tmp.path().join("absolute-storage");
        runtime.storage_dir = abs.to_string_lossy().to_string();

        let root = resolve_evolution_storage_root(&config, &runtime);
        assert_eq!(root, abs);
    }

    #[tokio::test]
    async fn load_evolution_config_defaults_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let (cfg, path) = load_evolution_config(&config).await.unwrap();
        assert_eq!(
            cfg.runtime.mode,
            crate::self_system::evolution::EvolutionMode::DraftOnly
        );
        assert_eq!(path, config.workspace_dir.join("evolution_config.toml"));
    }

    /// R-PMMCh S-1: a judge that gave no verdict marks the judge health
    /// component as an error, naming why; a later scored verdict clears it; a
    /// mock (degraded) verdict leaves the start-up status alone.
    #[test]
    fn a_failed_judge_verdict_marks_the_judge_component_error() {
        use crate::self_system::evolution::pipeline::JudgeOutcome;
        let component = "daemon-test-evolution-judge";
        crate::health::mark_component_ok(component);
        let failed = vec![
            "judge_failed: the model judge gave no verdict (the judge call timed out after 180s), so the verdict is \
             deferred"
                .to_string(),
        ];
        record_evolution_judge_health(
            component,
            [
                (JudgeOutcome::NotRun, &[][..]),
                (JudgeOutcome::Failed, failed.as_slice()),
            ],
        );
        let snapshot = crate::health::snapshot_json();
        assert_eq!(snapshot["components"][component]["status"], "error");
        assert!(
            snapshot["components"][component]["last_error"]
                .as_str()
                .unwrap_or("")
                .contains("timed out"),
            "{snapshot}"
        );

        record_evolution_judge_health(component, [(JudgeOutcome::Degraded, &[][..])]);
        assert_eq!(
            crate::health::snapshot_json()["components"][component]["status"],
            "error",
            "a mock verdict must not clear the judge error"
        );

        record_evolution_judge_health(component, [(JudgeOutcome::Scored, &[][..])]);
        assert_eq!(crate::health::snapshot_json()["components"][component]["status"], "ok");
    }

    /// D-046 / R-PMMCi F-1: `daemon::run` drains the evolution commit
    /// tracker after aborting its supervisors, and the daemon's pipelines
    /// run their verdict tasks on that tracker.
    #[test]
    fn daemon_run_drains_the_evolution_commit_tracker_before_returning() {
        let source = include_str!("mod.rs");
        let run = source
            .split_once("pub async fn run(")
            .and_then(|(_, rest)| rest.split_once("\nfn register_optional_component"))
            .expect("test: daemon::run body")
            .0;
        let abort = run.find("handle.abort()").expect("test: supervisors are aborted");
        let drain = run
            .find("drain_evolution_commits(crate::self_system::evolution::commit::daemon_commits())")
            .expect("daemon::run drains the evolution commit tracker");
        assert!(abort < drain, "the drain runs after the supervisors are aborted");
        let builder = source
            .split_once("async fn build_evolution_scheduler")
            .expect("test: scheduler builder")
            .1;
        assert!(
            builder.contains("with_commit_tracker(crate::self_system::evolution::commit::daemon_commits().clone())")
        );
    }

    #[tokio::test]
    async fn supervisor_marks_error_and_restart_on_failure() {
        let handle = spawn_component_supervisor("daemon-test-fail", 1, 1, || async { anyhow::bail!("boom") });

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["daemon-test-fail"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(component["last_error"].as_str().unwrap_or("").contains("boom"));
    }

    #[tokio::test]
    async fn supervisor_marks_unexpected_exit_as_error() {
        let handle = spawn_component_supervisor("daemon-test-exit", 1, 1, || async { Ok(()) });

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["daemon-test-exit"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(
            component["last_error"]
                .as_str()
                .unwrap_or("")
                .contains("component exited unexpectedly")
        );
    }

    #[tokio::test]
    async fn supervisor_does_not_treat_task_survival_as_readiness() {
        let handle = spawn_component_supervisor("daemon-test-pending", 1, 1, || async {
            std::future::pending::<Result<()>>().await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let snapshot = crate::health::snapshot_json();
        handle.abort();
        let _ = handle.await;

        assert_ne!(
            snapshot["components"]["daemon-test-pending"]["status"], "ok",
            "a pending task has not acknowledged readiness"
        );
    }

    fn next_test_generation(
        previous: &Arc<crate::config::ConfigGeneration>,
        id: u64,
    ) -> Arc<crate::config::ConfigGeneration> {
        Arc::new(crate::config::ConfigGeneration {
            id: crate::config::ConfigGenerationId(id),
            source_revision: previous.source_revision.clone(),
            effective: Arc::clone(&previous.effective),
            applied_at: Utc::now(),
            trigger: crate::config::ConfigReloadTrigger::Test,
            deferred_changes: Arc::from([]),
        })
    }

    async fn receive_generation_ack(
        receiver: std::sync::mpsc::Receiver<std::result::Result<(), String>>,
    ) -> std::result::Result<(), String> {
        crate::runtime::blocking::spawn_blocking(move || {
            receiver
                .recv_timeout(Duration::from_secs(2))
                .map_err(|error| error.to_string())?
        })
        .await
        .expect("ack task")
    }

    /// D-065 R-1: a hot reload to a default model that cannot chat is
    /// refused by the channels supervisor exactly as the gateway refuses it:
    /// the supervisor prepares a channel runtime for the candidate with the
    /// production field filter and runner (`start_channels_with_config`),
    /// whose start-up resolution fails, so the manager rejects the generation
    /// and the previous default stays active. Loopback and temp dirs only.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_channel_reload_to_a_model_that_cannot_chat_is_refused_and_the_previous_stays() {
        let workspace = tempfile::tempdir().expect("test: temp dir");
        let probe = crate::providers::credential_probe::CredentialProbe::start().await;
        let mut good = Config::default();
        good.workspace_dir = workspace.path().join("workspace");
        good.config_path = workspace.path().join("config.toml");
        std::fs::create_dir_all(&good.workspace_dir).expect("test: workspace");
        good.default_provider = Some(probe.custom_provider());
        good.default_model = Some("vendor/chat-model".to_string());
        good.memory.backend = "none".to_string();
        good.providers.models.push(
            toml::from_str(&format!(
                "provider = \"{}\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
                probe.custom_provider()
            ))
            .expect("test: declaration"),
        );
        let mut decision_only = good.clone();
        decision_only.default_model = Some("jev-decision-test".to_string());

        let manager = crate::config::new_shared(good);
        let root_shutdown = CancellationToken::new();
        let shared = Arc::clone(&manager);
        let runner: GenerationComponentRunner = Arc::new(move |generation, shutdown| {
            let shared = Arc::clone(&shared);
            Box::pin(async move {
                crate::channels::start_channels_with_config(
                    (*generation.effective).clone(),
                    shared,
                    generation,
                    shutdown,
                )
                .await
            })
        });
        let (handle, controller) = spawn_config_generation_supervisor(
            "channels",
            "channels",
            OPTIONAL_HEALTH_TTL_SECONDS,
            manager.pin(),
            Arc::new(channels_prepares_for_field),
            Arc::new(|_| true),
            runner,
            root_shutdown.clone(),
        );
        let alive_probe = controller.clone();
        let participant: Arc<dyn crate::config::ConfigGenerationParticipant> =
            Arc::new(DaemonConfigGenerationParticipant {
                controllers: vec![controller],
            });
        manager.register_participant(&participant);
        assert!(channels_prepares_for_field("default_model"));

        let applying = Arc::clone(&manager);
        let error = crate::runtime::blocking::spawn_blocking(move || {
            applying.apply_runtime_config(decision_only, crate::config::ConfigReloadTrigger::Test)
        })
        .await
        .expect("test: apply task")
        .expect_err("a default that cannot chat must not activate");

        let text = format!("{error:#}");
        assert!(text.contains("channels"), "{text}");
        assert!(text.contains("cannot be used for chat"), "{text}");
        assert_eq!(
            manager.pin().effective.default_model.as_deref(),
            Some("vendor/chat-model"),
            "the previous generation stays active"
        );
        assert!(
            !probe
                .requests()
                .iter()
                .any(|request| request.path.contains("chat/completions")),
            "no model request was sent"
        );
        // The channels supervisor survived the refusal and still answers: a
        // stale prepare is rejected rather than going unheard.
        let (stale_tx, stale_rx) = std::sync::mpsc::sync_channel(1);
        alive_probe
            .commands
            .send(GenerationSupervisorCommand::Prepare {
                generation: manager.pin(),
                response: stale_tx,
            })
            .expect("the channels supervisor is still running");
        let ack = receive_generation_ack(stale_rx).await;
        assert!(
            ack.as_ref().is_err_and(|error| error.contains("stale")),
            "the channels supervisor stopped answering after the refusal: {ack:?}"
        );

        root_shutdown.cancel();
        let _ = handle.await;
    }

    #[test]
    fn daemon_generation_participant_routes_only_matching_fields() {
        let (commands, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let participant = DaemonConfigGenerationParticipant {
            controllers: vec![GenerationSupervisorController {
                name: "cron-only",
                prepares_for_field: Arc::new(|field| field == "cron"),
                commands,
            }],
        };

        assert!(crate::config::ConfigGenerationParticipant::supports_controlled_restart_field(&participant, "cron"));
        assert!(crate::config::ConfigGenerationParticipant::prepares_for_field(
            &participant,
            "cron"
        ));
        assert!(
            !crate::config::ConfigGenerationParticipant::supports_controlled_restart_field(
                &participant,
                "channels_config"
            )
        );
        assert!(!crate::config::ConfigGenerationParticipant::prepares_for_field(
            &participant,
            "default_temperature"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generation_supervisor_fences_stale_commit_and_rolls_back_without_overlap() {
        let name = "daemon-generation-fencing-test";
        let root_shutdown = CancellationToken::new();
        let initial = crate::config::new_shared(Config::default()).pin();
        let starts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let runner: GenerationComponentRunner = {
            let starts = Arc::clone(&starts);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            Arc::new(move |generation, shutdown| {
                let starts = Arc::clone(&starts);
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                Box::pin(async move {
                    starts.lock().push(generation.id.0);
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    crate::health::mark_component_ok(name);
                    shutdown.cancelled().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
            })
        };
        let (handle, controller) = spawn_config_generation_supervisor(
            name,
            "test",
            60,
            Arc::clone(&initial),
            Arc::new(|_| true),
            Arc::new(|_| true),
            runner,
            root_shutdown.clone(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;

        let generation = next_test_generation(&initial, 1);
        let (prepare_tx, prepare_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .commands
            .send(GenerationSupervisorCommand::Prepare {
                generation,
                response: prepare_tx,
            })
            .unwrap();
        assert_eq!(receive_generation_ack(prepare_rx).await, Ok(()));

        let (stale_tx, stale_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .commands
            .send(GenerationSupervisorCommand::Commit {
                generation_id: 0,
                response: stale_tx,
            })
            .unwrap();
        assert!(receive_generation_ack(stale_rx).await.is_err());

        let (commit_tx, commit_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .commands
            .send(GenerationSupervisorCommand::Commit {
                generation_id: 1,
                response: commit_tx,
            })
            .unwrap();
        assert_eq!(receive_generation_ack(commit_rx).await, Ok(()));

        let (rollback_tx, rollback_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .commands
            .send(GenerationSupervisorCommand::Rollback {
                generation_id: 1,
                response: rollback_tx,
            })
            .unwrap();
        assert_eq!(receive_generation_ack(rollback_rx).await, Ok(()));

        root_shutdown.cancel();
        handle.await.unwrap();
        assert_eq!(*starts.lock(), vec![0, 1, 0]);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    /// The last error a component's health reports, if any.
    fn component_last_error(name: &str) -> Option<String> {
        crate::health::snapshot()
            .components
            .get(name)
            .and_then(|component| component.last_error.clone())
    }

    /// The component's last error once it mentions `needle`, polled until
    /// then (a minute at most, which only turns a hang into a failure).
    async fn wait_for_last_error(name: &str, needle: &str) -> String {
        for _ in 0..2400 {
            if let Some(error) = component_last_error(name).filter(|error| error.contains(needle)) {
                return error;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        component_last_error(name).unwrap_or_default()
    }

    /// A runner whose `n`-th start (0-based) behaves as `plan[n]`: `Ok` acks
    /// readiness and runs until shutdown, `Err(cause)` fails with `cause`
    /// under an outer context (so only `{:#}` shows it), and `ExitSoon` acks
    /// and then exits on its own after a moment.
    #[derive(Clone, Copy)]
    enum StartPlan {
        Run,
        Fail(&'static str),
        ExitSoon,
        /// Marks its own health failed with this cause and keeps running.
        MarkError(&'static str),
    }

    fn planned_runner(name: &'static str, plan: Vec<StartPlan>) -> GenerationComponentRunner {
        let calls = Arc::new(AtomicUsize::new(0));
        Arc::new(move |_generation, shutdown| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            let step = plan.get(call).copied().unwrap_or(StartPlan::Run);
            Box::pin(async move {
                match step {
                    StartPlan::Run => {
                        crate::health::mark_component_ok(name);
                        shutdown.cancelled().await;
                        Ok(())
                    }
                    StartPlan::Fail(cause) => Err(anyhow::anyhow!(cause).context("component setup failed")),
                    StartPlan::ExitSoon => {
                        crate::health::mark_component_ok(name);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(())
                    }
                    StartPlan::MarkError(cause) => {
                        crate::health::mark_component_error(name, cause);
                        shutdown.cancelled().await;
                        Ok(())
                    }
                }
            })
        })
    }

    /// D-067, RED before: the initial-generation failure was recorded with
    /// `to_string()`, which keeps only the outer context, so the component's
    /// health did not say why it failed. After: the whole chain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_initial_generation_failure_reports_the_whole_error_chain() {
        let name = "daemon-generation-initial-chain-test";
        let root_shutdown = CancellationToken::new();
        let initial = crate::config::new_shared(Config::default()).pin();
        let (handle, _controller) = spawn_config_generation_supervisor(
            name,
            "test",
            60,
            initial,
            Arc::new(|_| true),
            Arc::new(|_| true),
            planned_runner(name, vec![StartPlan::Fail("INITIAL-ROOT-CAUSE")]),
            root_shutdown.clone(),
        );
        let error = wait_for_last_error(name, "INITIAL-ROOT-CAUSE").await;
        assert!(error.contains("INITIAL-ROOT-CAUSE"), "{error}");
        root_shutdown.cancel();
        handle.await.unwrap();
    }

    /// D-072 L-3: a chain logged whole may carry an inner error's secret (a
    /// token in a request URL); the log copy is redacted and keeps the chain.
    #[test]
    fn a_logged_error_chain_is_redacted() {
        let error = Err::<(), _>(anyhow::anyhow!(
            "error sending request for url (https://user:hunter2@api.example.test/v1?key=sk-abc123456789012345678901)"
        ))
        .context("provider warmup failed")
        .expect_err("test: an error");
        let logged = redacted_chain(&error);
        assert!(
            logged.starts_with("provider warmup failed: error sending request"),
            "{logged}"
        );
        assert!(!logged.contains("hunter2"), "{logged}");
        assert!(!logged.contains("sk-abc123456789012345678901"), "{logged}");
    }

    /// D-072 L-2, RED before: a component that marked its own health failed
    /// during preparation had that cause overwritten by the supervisor's
    /// "reported failed health" message. After: the cause is kept in it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_component_that_marks_itself_failed_keeps_its_own_cause() {
        let name = "daemon-generation-self-marked-test";
        let root_shutdown = CancellationToken::new();
        let initial = crate::config::new_shared(Config::default()).pin();
        let (handle, _controller) = spawn_config_generation_supervisor(
            name,
            "test",
            60,
            initial,
            Arc::new(|_| true),
            Arc::new(|_| true),
            planned_runner(name, vec![StartPlan::MarkError("SELF-MARKED-CAUSE")]),
            root_shutdown.clone(),
        );
        let error = wait_for_last_error(name, "reported failed health").await;
        assert!(
            error.contains("reported failed health") && error.contains("SELF-MARKED-CAUSE"),
            "{error}"
        );
        root_shutdown.cancel();
        handle.await.unwrap();
    }

    /// D-067, RED before: a restart after the component exited recorded only
    /// the outer context of a failed start. After: the whole chain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_restart_tick_reports_the_whole_error_chain() {
        let name = "daemon-generation-restart-chain-test";
        let root_shutdown = CancellationToken::new();
        let initial = crate::config::new_shared(Config::default()).pin();
        let (handle, _controller) = spawn_config_generation_supervisor(
            name,
            "test",
            60,
            initial,
            Arc::new(|_| true),
            Arc::new(|_| true),
            planned_runner(name, vec![StartPlan::ExitSoon, StartPlan::Fail("RESTART-ROOT-CAUSE")]),
            root_shutdown.clone(),
        );
        let mut error = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            error = component_last_error(name).unwrap_or_default();
            if error.contains("RESTART-ROOT-CAUSE") {
                break;
            }
        }
        assert!(error.contains("RESTART-ROOT-CAUSE"), "{error}");
        root_shutdown.cancel();
        handle.await.unwrap();
    }

    /// D-067, RED before: a `Rollback` whose restore of the previous
    /// generation failed replied with the outer context only and left the
    /// component's health silent although it was down. After: the reply has
    /// the whole chain and the health is failed with the cause.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_rollback_reports_the_chain_and_marks_the_component_failed() {
        let name = "daemon-generation-rollback-chain-test";
        let root_shutdown = CancellationToken::new();
        let initial = crate::config::new_shared(Config::default()).pin();
        let (handle, controller) = spawn_config_generation_supervisor(
            name,
            "test",
            60,
            Arc::clone(&initial),
            Arc::new(|_| true),
            Arc::new(|_| true),
            planned_runner(
                name,
                vec![StartPlan::Run, StartPlan::Run, StartPlan::Fail("ROLLBACK-ROOT-CAUSE")],
            ),
            root_shutdown.clone(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (prepare_tx, prepare_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .commands
            .send(GenerationSupervisorCommand::Prepare {
                generation: next_test_generation(&initial, 1),
                response: prepare_tx,
            })
            .unwrap();
        assert_eq!(receive_generation_ack(prepare_rx).await, Ok(()));

        let (rollback_tx, rollback_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .commands
            .send(GenerationSupervisorCommand::Rollback {
                generation_id: 1,
                response: rollback_tx,
            })
            .unwrap();
        let reply = receive_generation_ack(rollback_rx)
            .await
            .expect_err("test: the restore fails");
        assert!(reply.contains("ROLLBACK-ROOT-CAUSE"), "{reply}");
        let health = crate::health::snapshot();
        let component = health.components.get(name).expect("test: registered");
        assert_eq!(component.state, crate::health::ComponentState::Failed);
        let error = component.last_error.clone().unwrap_or_default();
        assert!(error.contains("ROLLBACK-ROOT-CAUSE"), "{error}");
        root_shutdown.cancel();
        handle.await.unwrap();
    }

    /// D-067, RED before: when a candidate generation failed and the previous
    /// generation also failed to restart, only a log line said so; the
    /// component stayed down with no failed health. After: its health is
    /// failed and names the restore failure (the reply names the candidate's).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_candidate_whose_restore_also_fails_marks_the_component_failed() {
        let name = "daemon-generation-restore-failure-test";
        let root_shutdown = CancellationToken::new();
        let initial = crate::config::new_shared(Config::default()).pin();
        let (handle, controller) = spawn_config_generation_supervisor(
            name,
            "test",
            60,
            Arc::clone(&initial),
            Arc::new(|_| true),
            Arc::new(|_| true),
            planned_runner(
                name,
                vec![
                    StartPlan::Run,
                    StartPlan::Fail("CANDIDATE-ROOT-CAUSE"),
                    StartPlan::Fail("RESTORE-ROOT-CAUSE"),
                ],
            ),
            root_shutdown.clone(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (prepare_tx, prepare_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .commands
            .send(GenerationSupervisorCommand::Prepare {
                generation: next_test_generation(&initial, 1),
                response: prepare_tx,
            })
            .unwrap();
        let reply = receive_generation_ack(prepare_rx)
            .await
            .expect_err("test: the candidate fails");
        assert!(reply.contains("CANDIDATE-ROOT-CAUSE"), "{reply}");
        let health = crate::health::snapshot();
        let component = health.components.get(name).expect("test: registered");
        assert_eq!(component.state, crate::health::ComponentState::Failed);
        let error = component.last_error.clone().unwrap_or_default();
        assert!(error.contains("could not restart"), "{error}");
        assert!(error.contains("RESTORE-ROOT-CAUSE"), "{error}");
        root_shutdown.cancel();
        handle.await.unwrap();
    }

    /// D-072 L-5, RED before: when the candidate failed and the previous
    /// generation could not be restored either, the component stayed down
    /// until the next config generation (the tick restarts only a task that
    /// finished, and there was none). After: it is retried with a backoff and
    /// comes back once a start succeeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_component_whose_restore_failed_is_retried_until_it_runs() {
        let name = "daemon-generation-restore-retry-test";
        let root_shutdown = CancellationToken::new();
        let initial = crate::config::new_shared(Config::default()).pin();
        let (handle, controller) = spawn_config_generation_supervisor(
            name,
            "test",
            60,
            Arc::clone(&initial),
            Arc::new(|_| true),
            Arc::new(|_| true),
            planned_runner(
                name,
                vec![
                    StartPlan::Run,
                    StartPlan::Fail("CANDIDATE-ROOT-CAUSE"),
                    StartPlan::Fail("RESTORE-ROOT-CAUSE"),
                    StartPlan::Fail("FIRST-RETRY-ROOT-CAUSE"),
                    StartPlan::Run,
                ],
            ),
            root_shutdown.clone(),
        );
        let (prepare_tx, prepare_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .commands
            .send(GenerationSupervisorCommand::Prepare {
                generation: next_test_generation(&initial, 1),
                response: prepare_tx,
            })
            .unwrap();
        assert!(receive_generation_ack(prepare_rx).await.is_err());

        let mut state = None;
        for _ in 0..2400 {
            state = crate::health::snapshot()
                .components
                .get(name)
                .map(|component| component.state);
            if state == Some(crate::health::ComponentState::Ready) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            state,
            Some(crate::health::ComponentState::Ready),
            "the component comes back after a failed retry"
        );
        root_shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn generation_component_requires_explicit_health_readiness_ack() {
        let generation = crate::config::new_shared(Config::default()).pin();
        let runner: GenerationComponentRunner = Arc::new(|_, shutdown| {
            Box::pin(async move {
                shutdown.cancelled().await;
                Ok(())
            })
        });
        let enabled: GenerationComponentEnabled = Arc::new(|_| true);

        let error = start_generation_component(
            "daemon-generation-no-readiness-test",
            "test",
            60,
            generation,
            &enabled,
            &runner,
        )
        .await
        .expect_err("task survival without health ack must not count as readiness");

        assert!(error.to_string().contains("did not acknowledge readiness"));
    }

    #[tokio::test]
    async fn generation_activation_waits_until_manager_publishes_candidate() {
        let manager = crate::config::new_shared(Config::default());
        let waiter = tokio::spawn({
            let manager = Arc::clone(&manager);
            async move {
                wait_for_active_generation(&manager, crate::config::ConfigGenerationId(1)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(
            !waiter.is_finished(),
            "candidate work admitted before active publication"
        );

        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.16;
        manager
            .apply_runtime_config(desired, crate::config::ConfigReloadTrigger::Test)
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("activation waiter timed out")
            .unwrap();
    }

    #[test]
    fn detects_no_supervised_channels() {
        let config = Config::default();
        assert!(!has_supervised_channels(&config));
    }

    #[test]
    fn detects_supervised_channels_present() {
        let mut config = Config::default();
        config.channels_config.telegram = Some(crate::config::TelegramConfig {
            bot_token: "token".into(),
            allowed_users: vec![],
            stream_mode: crate::config::StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
            group_reply_mode: None,
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_dingtalk_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.dingtalk = Some(crate::config::schema::DingTalkConfig {
            client_id: "client_id".into(),
            client_secret: "client_secret".into(),
            allowed_users: vec!["*".into()],
            mention_only: false,
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_mattermost_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.mattermost = Some(crate::config::schema::MattermostConfig {
            url: "https://mattermost.example.com".into(),
            bot_token: "token".into(),
            channel_id: Some("channel-id".into()),
            allowed_users: vec!["*".into()],
            thread_replies: Some(true),
            mention_only: Some(false),
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_qq_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.qq = Some(crate::config::schema::QQConfig {
            app_id: "app-id".into(),
            app_secret: "app-secret".into(),
            allowed_users: vec!["*".into()],
            mention_only: false,
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_nextcloud_talk_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.nextcloud_talk = Some(crate::config::schema::NextcloudTalkConfig {
            base_url: "https://cloud.example.com".into(),
            app_token: "app-token".into(),
            webhook_secret: None,
            allowed_users: vec!["*".into()],
            mention_only: false,
        });
        assert!(has_supervised_channels(&config));
    }

    /// USERINFO-REDACT: a judge provider that fails to build is recorded on
    /// the health snapshot `GET /health` serializes. The default provider may
    /// be a `custom:` URL carrying `user:password@`.
    #[test]
    fn a_degraded_judge_names_its_provider_without_url_userinfo() {
        let _serial = JUDGE_HEALTH_SERIAL.lock();
        let config = Config {
            // Port 99999 does not parse, so the chain cannot be built.
            default_provider: Some("custom:http://alice:SECRET-fixture-pw@127.0.0.1:99999/v1".to_string()),
            api_key: Some("sk-fixture-not-real".to_string()),
            ..Config::default()
        };
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let sink = std::sync::Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || JudgeLogCapture(std::sync::Arc::clone(&sink)))
            .finish();
        let _judge = tracing::subscriber::with_default(subscriber, || build_evolution_judge_model(&config));
        let logged = String::from_utf8_lossy(&captured.lock()).into_owned();
        assert!(
            logged.contains("DEGRADED"),
            "the capture saw no degraded-judge warning: {logged}"
        );
        assert!(
            !logged.contains("SECRET-fixture-pw"),
            "the warning leaked the password: {logged}"
        );
        let snapshot = serde_json::to_string(&crate::health::snapshot()).expect("test: snapshot serializes");
        assert!(
            snapshot.contains("custom:http://[redacted]@127.0.0.1:99999/v1"),
            "the degraded judge must still name its endpoint: {snapshot}"
        );
        assert!(!snapshot.contains("SECRET-fixture-pw"), "{snapshot}");
    }

    /// The judge tests write the process-wide `evolution_judge` health
    /// component; one runs at a time so each reads its own record.
    static JUDGE_HEALTH_SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// PMM-C: a default that cannot chat degrades the judge to the mock
    /// before any provider is built, and says why.
    #[test]
    fn a_decision_only_default_degrades_the_judge_before_building() {
        let _serial = JUDGE_HEALTH_SERIAL.lock();
        let mut config = Config {
            default_provider: Some("openrouter".to_string()),
            default_model: Some("jev-decision-test".to_string()),
            api_key: Some("sk-fixture-not-real".to_string()),
            ..Config::default()
        };
        config.providers.models.push(
            toml::from_str(
                "provider = \"openrouter\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
            )
            .expect("test: declaration"),
        );
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let sink = std::sync::Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || JudgeLogCapture(std::sync::Arc::clone(&sink)))
            .finish();
        let _judge = tracing::subscriber::with_default(subscriber, || build_evolution_judge_model(&config));
        let logged = String::from_utf8_lossy(&captured.lock()).into_owned();
        assert!(
            logged.contains("DEGRADED") && logged.contains("cannot be used for chat"),
            "a decision-only default built a real judge: {logged}"
        );
        assert!(!logged.contains("sk-fixture-not-real"), "{logged}");
    }

    /// Finding 64: the daemon's real evolution-judge factory retains model
    /// fallback dialect facts and degrades before any provider request.
    #[tokio::test]
    async fn evolution_judge_refuses_a_cross_dialect_fallback_before_transport() {
        let probe = crate::providers::credential_probe::CredentialProbe::start().await;
        let temp = tempfile::tempdir().expect("test: temp workspace");
        let mut config = Config {
            workspace_dir: temp.path().to_path_buf(),
            ..Config::default()
        };
        crate::providers::entrypoint_consistency_tests::configure_cross_dialect(&mut config, &probe.custom_provider());
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let sink = std::sync::Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || JudgeLogCapture(std::sync::Arc::clone(&sink)))
            .finish();
        {
            let _serial = JUDGE_HEALTH_SERIAL.lock();
            let _judge = tracing::subscriber::with_default(subscriber, || build_evolution_judge_model(&config));
        }
        let logged = String::from_utf8_lossy(&captured.lock()).into_owned();

        assert!(logged.contains("DEGRADED"), "the judge did not degrade: {logged}");
        crate::providers::entrypoint_consistency_tests::assert_cross_dialect_refusal("evolution judge", &logged);
        assert!(probe.requests().is_empty(), "the judge reached transport: {logged}");
    }

    /// A loopback chat endpoint that records the `model` of every request
    /// and answers with a judge score.
    async fn judge_stub() -> (
        String,
        std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        let models: std::sync::Arc<parking_lot::Mutex<Vec<String>>> = std::sync::Arc::default();
        let sink = std::sync::Arc::clone(&models);
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 8192];
                while let Ok(read) = socket.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buffer).to_string();
                    let Some(split) = text.find("\r\n\r\n") else { continue };
                    let length = text[..split]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if buffer.len() >= split + 4 + length {
                        if let Ok(body) =
                            serde_json::from_slice::<serde_json::Value>(&buffer[split + 4..split + 4 + length])
                        {
                            sink.lock().push(body["model"].as_str().unwrap_or_default().to_string());
                        }
                        break;
                    }
                }
                let content = "{\"task_completion\":0.7,\"correctness\":0.7,\"efficiency\":0.7,\"safety_compliance\":0.7,\"reasoning\":\"r\"}";
                let payload = serde_json::json!({
                    "id": "x", "object": "chat.completion",
                    "usage": {"prompt_tokens": 20, "completion_tokens": 10, "total_tokens": 30},
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("custom:http://{addr}/v1"), models, server)
    }

    /// D-034 / R-PMMC F-3: the judge sends the resolved default model, never
    /// the `mock-judge-v1` placeholder, to a real provider.
    #[tokio::test]
    async fn the_judge_sends_the_resolved_default_model_never_the_placeholder() {
        use crate::self_system::evolution::judge::JudgeConfig;
        let (label, models, server) = judge_stub().await;
        let workspace = tempfile::tempdir().unwrap();
        let mut config = Config {
            workspace_dir: workspace.path().to_path_buf(),
            default_provider: Some(label.clone()),
            default_model: Some("judge-model".to_string()),
            ..Config::default()
        };
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": label, "model": "judge-model", "operations": ["chat"], "native_tools": false
            }))
            .unwrap(),
        );
        config.cost.prices.insert(
            format!("{label}/judge-model"),
            crate::config::schema::ModelPricing {
                input: 0.0,
                output: 0.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let judge = {
            let _serial = JUDGE_HEALTH_SERIAL.lock();
            build_evolution_judge_model(&config)
        };
        assert!(judge.score("task", "done", &JudgeConfig::default()).await.is_err());
        assert!(
            models.lock().is_empty(),
            "missing cleanup ownership must prevent transport"
        );
        let engine = crate::self_system::evolution::judge::JudgeEngine::new(JudgeConfig::default(), judge);
        let verdict = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
            .unwrap()
            .run(async { engine.judge_task("experiment", "task", "task", "done").await })
            .await
            .unwrap();
        let journal = std::fs::read_to_string(workspace.path().join("state/costs.reservations.jsonl")).unwrap();
        let provenance = verdict.model_provenance.unwrap();
        assert_eq!(provenance.model.model, "judge-model");
        assert!(provenance.usage.reported);
        assert!(journal.contains(&provenance.accounting_scope_id.unwrap()));
        assert!(journal.contains("evolution_judge"));
        assert!(journal.contains("estimated_settled_v1"));
        let seen = models.lock().clone();
        server.abort();
        assert!(
            !seen.iter().any(|model| model == "mock-judge-v1"),
            "the placeholder reached the provider: {seen:?}"
        );
        assert_eq!(seen.first().map(String::as_str), Some("judge-model"), "{seen:?}");
    }

    struct JudgeLogCapture(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for JudgeLogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
