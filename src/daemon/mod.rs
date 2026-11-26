#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::config::{Config, HotReloadManager, new_shared};
use crate::self_system::evolution::{
    AsyncJsonlWriter, EvolutionAnalyzer, EvolutionConfig, EvolutionPipeline, EvolutionRetentionConfig,
    EvolutionRuntimeConfig, EvolutionScheduler, JsonlRetentionPolicy, JsonlStoragePaths, PromptEvolutionEngine,
    new_shared_evolution_config,
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
        // Always available: retry within seconds, never a minute.
        handles.push(spawn_component_supervisor(
            "gateway",
            initial_backoff,
            max_backoff.min(crate::config::schema::GATEWAY_RESTART_MAX_BACKOFF_SECS),
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
        evolution_scheduler_enable_predicate(),
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
        let first_backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(first_backoff);
        let mut backoff = first_backoff;
        // Restarts since the component last ran healthy.
        let mut attempt: u32 = 0;

        loop {
            crate::health::mark_component_starting(name);
            let started = tokio::time::Instant::now();
            match run_component().await {
                Ok(()) => {
                    crate::health::mark_component_error(name, "component exited unexpectedly");
                    tracing::warn!("Daemon component '{name}' exited unexpectedly");
                    // Clean exit — reset backoff since the component ran successfully
                    backoff = first_backoff;
                    attempt = 0;
                }
                Err(e) => {
                    // The whole chain: the outer context alone ("… failed")
                    // does not say why (D-067).
                    crate::health::mark_component_error(name, format!("{e:#}"));
                    tracing::error!("Daemon component '{name}' failed: {}", redacted_chain(&e));
                }
            }

            // A run that outlived the current backoff was healthy: it forgives
            // the failures before it, so the next restart starts over.
            if started.elapsed() > Duration::from_secs(backoff) {
                backoff = first_backoff;
                attempt = 0;
            }
            attempt = attempt.saturating_add(1);

            crate::health::bump_component_restart(name);
            tracing::warn!(
                component = name,
                attempt,
                delay_secs = backoff,
                "Restarting daemon component after backoff"
            );
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

const fn evolution_scheduler_enabled(config: &Config) -> bool {
    config.skills_lifecycle.enabled
}

fn evolution_scheduler_enable_predicate() -> GenerationComponentEnabled {
    Arc::new(evolution_scheduler_enabled)
}

async fn build_evolution_scheduler(config: &Config) -> Result<(EvolutionScheduler, u64)> {
    let (cfg, _cfg_path) = load_evolution_config(config).await?;
    tokio::fs::create_dir_all(&config.workspace_dir)
        .await
        .with_context(|| {
            format!(
                "failed to prepare workspace dir for evolution scheduler: {}",
                config.workspace_dir.display()
            )
        })?;

    #[cfg(feature = "wasm-plugins")]
    let _wasm_early_runtime = if config
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
    let storage_root = resolve_evolution_storage_root(config, &cfg.runtime)?;
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

    let mut prompt_engine = PromptEvolutionEngine::new(shared.clone(), &config.workspace_dir, Some(writer))
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
    let scheduler = EvolutionScheduler::new(
        shared,
        analyzer,
        pipeline,
        config.workspace_dir.join(".evolution/scheduler_state.json"),
        prompt_engine,
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
    use crate::health::ComponentState;
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
                let reason = crate::providers::redact_endpoint_userinfo(reason).into_owned();
                crate::health::mark_component_advisory(component, ComponentState::Failed, Some(reason));
                return;
            }
            JudgeOutcome::Scored => scored = true,
            JudgeOutcome::NotRun | JudgeOutcome::Degraded => {}
        }
    }
    if scored {
        crate::health::mark_component_advisory(component, ComponentState::Ready, None);
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
    use crate::health::ComponentState;
    use crate::self_system::evolution::judge::{ManagedJudge, MockJudgeModel};
    let provider_name = crate::providers::selection::default_provider(config).to_string();
    let built = crate::runtime::evolution_model::EvolutionModel::from_config(
        config,
        crate::runtime::evolution_model::EvolutionPurpose::Judge,
    );
    match built {
        Ok(model) => {
            crate::health::mark_component_advisory(EVOLUTION_JUDGE_COMPONENT, ComponentState::Ready, None);
            Arc::new(ManagedJudge(model))
        }
        Err(error) => {
            // The default provider may be a `custom:` URL carrying
            // `user:password@`; the health record and the log name the
            // endpoint without it.
            let provider_name = crate::providers::redact_endpoint_userinfo(&provider_name);
            let error = crate::providers::redact_endpoint_userinfo(&error.to_string()).into_owned();
            let reason = format!("provider '{provider_name}' unavailable: {error}");
            crate::health::mark_component_advisory(EVOLUTION_JUDGE_COMPONENT, ComponentState::Failed, Some(reason));
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

fn resolve_evolution_storage_root(config: &Config, runtime: &EvolutionRuntimeConfig) -> anyhow::Result<PathBuf> {
    let root = Path::new(&runtime.storage_dir);
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        config.workspace_dir.join(root)
    };
    let self_dir = config.workspace_dir.join("self");
    if root.starts_with(&self_dir) {
        crate::util::create_private_dir_all(&self_dir)?;
    }
    Ok(root)
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
        ramflux,
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
        || ramflux.is_some()
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
mod tests;
