#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::config::{Config, HotReloadManager, new_shared};
use crate::self_system::evolution::{
    AsyncJsonlWriter, EvolutionAnalyzer, EvolutionConfig, EvolutionPipeline, EvolutionRetentionConfig,
    EvolutionRuntimeConfig, EvolutionScheduler, JsonlRetentionPolicy, JsonlStoragePaths, MemoryEvolutionEngine,
    PromptEvolutionEngine, StrategyEvolutionEngine, new_shared_evolution_config,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

const STATUS_FLUSH_SECONDS: u64 = 5;
const MANUAL_DAEMON_STALE_SECONDS: i64 = 30;
const CORE_HEALTH_TTL_SECONDS: u64 = 60;
const OPTIONAL_HEALTH_TTL_SECONDS: u64 = 300;

pub async fn run(config: Config, host: String, port: u16, shutdown: CancellationToken) -> Result<()> {
    ensure_manual_daemon_start_allowed(&config)?;

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

    if config.modules.scheduler && config.heartbeat.enabled {
        let _ = crate::heartbeat::engine::HeartbeatEngine::ensure_heartbeat_file(&config.workspace_dir).await;
    }

    let mut handles: Vec<JoinHandle<()>> = vec![spawn_state_writer(config.clone(), Arc::clone(&shared_config))];

    // Gateway always starts — modules.network only controls whether network.toml
    // is loaded, not the gateway itself.
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

    let initial_generation = shared_config.pin();
    let mut generation_controllers = Vec::new();

    let (channels_handle, channels_controller) = spawn_config_generation_supervisor(
        "channels",
        "channels",
        OPTIONAL_HEALTH_TTL_SECONDS,
        Arc::clone(&initial_generation),
        Arc::new(|field| field == "channels_config" || crate::config::generation::is_rebuild_and_swap(field)),
        Arc::new(|cfg| cfg.modules.channels && has_supervised_channels(cfg)),
        {
            let shared = Arc::clone(&shared_config);
            Arc::new(move |generation, shutdown| {
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
        Arc::new(|cfg| cfg.modules.scheduler && cfg.cron.enabled),
        {
            let shared = Arc::clone(&shared_config);
            Arc::new(move |generation, shutdown| {
                let shared = Arc::clone(&shared);
                Box::pin(async move {
                    let cfg = (*generation.effective).clone();
                    tokio::select! {
                        result = crate::cron::scheduler::run_with_config_generation_manager(
                            cfg,
                            generation,
                            shared,
                        ) => result,
                        () = shutdown.cancelled() => Ok(()),
                    }
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
            Arc::new(move |generation, shutdown| {
                let shared = Arc::clone(&shared);
                Box::pin(async move {
                    let cfg = (*generation.effective).clone();
                    tokio::select! {
                        result = crate::xin::runner::run_with_config_generation_manager(
                            cfg,
                            generation,
                            shared,
                        ) => result,
                        () = shutdown.cancelled() => Ok(()),
                    }
                })
            })
        },
        shutdown.clone(),
    );
    handles.push(xin_handle);
    generation_controllers.push(xin_controller);

    let fitness_ttl = config
        .self_system
        .fitness_interval_hours
        .saturating_mul(7200)
        .max(OPTIONAL_HEALTH_TTL_SECONDS);
    let (fitness_handle, fitness_controller) = spawn_config_generation_supervisor(
        "self_system_fitness",
        "self-system",
        fitness_ttl,
        Arc::clone(&initial_generation),
        Arc::new(|field| field == "self_system"),
        Arc::new(|cfg| {
            let xin_manages =
                cfg.modules.scheduler && cfg.xin.enabled && cfg.xin.builtin_tasks && cfg.xin.evolution_integration;
            cfg.self_system.enabled && !xin_manages
        }),
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
        Arc::new(|cfg| {
            let xin_manages =
                cfg.modules.scheduler && cfg.xin.enabled && cfg.xin.builtin_tasks && cfg.xin.evolution_integration;
            cfg.self_system.evolution_enabled && !xin_manages
        }),
        {
            let shared = Arc::clone(&shared_config);
            Arc::new(move |generation, shutdown| {
                let shared = Arc::clone(&shared);
                Box::pin(async move {
                    tokio::select! {
                        result = run_evolution_scheduler_worker(
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
    handles.push(evolution_handle);
    generation_controllers.push(evolution_controller);

    let (webhook_handle, webhook_controller) = spawn_config_generation_supervisor(
        "webhook_receiver",
        "webhook",
        OPTIONAL_HEALTH_TTL_SECONDS,
        initial_generation,
        Arc::new(|field| field == "webhook" || crate::config::generation::is_rebuild_and_swap(field)),
        Arc::new(|cfg| cfg.modules.integrations && cfg.webhook.enabled),
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
    register_optional_component(
        "heartbeat",
        "xin",
        config.modules.scheduler && config.heartbeat.enabled,
        heartbeat_ttl,
    );

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
        // The abort-based teardown below is preserved verbatim: the daemon performs
        // a stateless exit and must not be turned into a graceful child-await.
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

const fn xin_runtime_enabled(config: &Config) -> bool {
    config.modules.scheduler && (config.xin.enabled || config.heartbeat.enabled)
}

fn ensure_manual_daemon_start_allowed(config: &Config) -> Result<()> {
    if is_systemd_managed_start() {
        return Ok(());
    }

    let state_path = state_file_path(config);
    let Some(active) = active_daemon_from_state_file(&state_path) else {
        return Ok(());
    };

    anyhow::bail!(
        "Refusing to start `prx daemon` outside systemd because an active daemon appears to be running \
         (pid {}, state file {}). Use `systemctl --user restart prx.service` to restart it, or \
         `systemctl --user stop prx.service` before manual debugging.",
        active.pid,
        state_path.display()
    )
}

fn is_systemd_managed_start() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
        || std::env::var_os("JOURNAL_STREAM").is_some()
        || std::env::var_os("NOTIFY_SOCKET").is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveDaemonState {
    pid: u32,
}

fn active_daemon_from_state_file(path: &Path) -> Option<ActiveDaemonState> {
    let raw = std::fs::read_to_string(path).ok()?;
    active_daemon_from_state_json(&raw, Utc::now())
}

fn active_daemon_from_state_json(raw: &str, now: DateTime<Utc>) -> Option<ActiveDaemonState> {
    let json: serde_json::Value = serde_json::from_str(raw).ok()?;
    let pid = json.get("pid")?.as_u64().and_then(|value| u32::try_from(value).ok())?;

    let daemon_last_ok = json
        .get("components")
        .and_then(|components| components.get("daemon"))
        .and_then(|daemon| daemon.get("last_ok"))
        .and_then(serde_json::Value::as_str);
    let written_at = json.get("written_at").and_then(serde_json::Value::as_str);

    if !is_recent_timestamp(daemon_last_ok, now) && !is_recent_timestamp(written_at, now) {
        return None;
    }
    if !process_appears_alive(pid) {
        return None;
    }

    Some(ActiveDaemonState { pid })
}

fn is_recent_timestamp(raw: Option<&str>, now: DateTime<Utc>) -> bool {
    let Some(raw) = raw else {
        return false;
    };
    let Ok(parsed) = DateTime::parse_from_rfc3339(raw) else {
        return false;
    };
    let age = now.signed_duration_since(parsed.with_timezone(&Utc));
    age.num_seconds() >= 0 && age.num_seconds() <= MANUAL_DAEMON_STALE_SECONDS
}

#[cfg(target_os = "linux")]
fn process_appears_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
const fn process_appears_alive(_pid: u32) -> bool {
    true
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
                    crate::health::mark_component_error(name, e.to_string());
                    tracing::error!("Daemon component '{name}' failed: {e}");
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
            crate::health::mark_component_error(name, error.to_string());
            tracing::error!(component = name, "initial generation failed: {error}");
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
                                        tracing::error!(
                                            component = name,
                                            "candidate failed and old generation could not restart: {rollback_error}"
                                        );
                                    }
                                }
                                let _ = response.send(Err(error.to_string()));
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
                                let _ = response.send(Err(error.to_string()));
                            }
                        }
                    }
                }
            }
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                if task.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
                    if let Some(finished) = task.take() {
                        match finished.await {
                            Ok(Ok(())) => tracing::warn!(component = name, "component exited unexpectedly"),
                            Ok(Err(error)) => tracing::error!(component = name, "component failed: {error}"),
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
                        Err(error) => crate::health::mark_component_error(name, error.to_string()),
                    }
                }
            }
        }
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
                            return Err(anyhow::anyhow!(
                                "{name} reported failed health during generation preparation"
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
            handle.abort();
            let _ = handle.await;
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
    let interval_hours = config.self_system.fitness_interval_hours.max(1);
    let mut interval = tokio::time::interval(Duration::from_secs(interval_hours.saturating_mul(3600)));
    crate::health::mark_component_ok("self_system_fitness");
    wait_for_active_generation(&manager, generation_id).await;

    loop {
        interval.tick().await;
        match crate::self_system::run_fitness_report_with_config(&config).await {
            Ok(report) => {
                crate::health::mark_component_ok("self_system_fitness");
                tracing::info!(
                    target: "self_system",
                    "fitness report stored: score={:.3}, confidence={:.3}, date={}",
                    report.final_score,
                    report.confidence,
                    report.window.date
                );
            }
            Err(error) => {
                crate::health::mark_component_error("self_system_fitness", error.to_string());
                tracing::warn!(
                    target: "self_system",
                    "fitness report failed: {error}"
                );
            }
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
    crate::health::mark_component_ok("evolution_scheduler");
    wait_for_active_generation(&manager, generation_id).await;

    loop {
        interval.tick().await;
        match scheduler.run_scheduled(Utc::now()).await {
            Ok(summary) => {
                if summary.digest_ran || summary.cycle_ran {
                    // FIX-P0-40: report how many layers were skipped by the
                    // side-effect gate so a "completed" tick that applied nothing
                    // (because autonomy denied every self-modification) is not
                    // silently indistinguishable from one that applied changes.
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
                crate::health::mark_component_ok("evolution_scheduler");
            }
            Err(error) => {
                tracing::warn!(
                    target: "self_system",
                    "evolution scheduler tick failed: {error}"
                );
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
    // LLM-backed `ModelJudge` (falling back to the deterministic mock only when
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
    .with_security_policy(security_policy);

    let evolution_memory: Arc<dyn crate::memory::Memory> =
        Arc::from(crate::memory::create_memory_with_storage_and_routes_with_acl(
            &config.memory,
            &config.embedding_routes,
            Some(&config.storage.provider.config),
            &config.workspace_dir,
            config.api_key.as_deref(),
            &config.identity_bindings,
            &config.user_policies,
        )?);
    let memory_engine = Box::new(
        MemoryEvolutionEngine::new(shared.clone(), &cfg_path, Some(writer.clone()))
            .with_context(|| format!("failed to initialize memory evolution engine: {}", cfg_path.display()))?
            .with_memory(evolution_memory),
    );
    let prompt_engine = Box::new(
        PromptEvolutionEngine::new(shared.clone(), &config.workspace_dir, Some(writer.clone()))
            .with_debug_raw(config.self_system.evolution_debug_raw),
    );
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

/// Health component name reporting whether evolution judging runs on a real
/// LLM judge or has degraded to the deterministic mock judge (FIX-P1-10).
const EVOLUTION_JUDGE_COMPONENT: &str = "evolution_judge";

/// FIX-P1-10: build the scoring model used to judge evolution cycles.
///
/// The production path prefers a real LLM-backed [`ModelJudge`] driven by the
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
    use crate::self_system::evolution::judge::{MockJudgeModel, ModelJudge};

    let provider_name = config
        .default_provider
        .clone()
        .unwrap_or_else(|| "openrouter".to_string());
    let provider_runtime_options = crate::providers::provider_runtime_options_from_config(config);

    match crate::providers::create_resilient_provider_with_options(
        &provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &provider_runtime_options,
    ) {
        Ok(provider) => {
            let provider: Arc<dyn crate::providers::traits::Provider> = Arc::from(provider);
            crate::health::mark_component_ok(EVOLUTION_JUDGE_COMPONENT);
            Arc::new(ModelJudge::new(provider))
        }
        Err(error) => {
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

const fn has_supervised_channels(config: &Config) -> bool {
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

    telegram.is_some()
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
        || matches!(wacli, Some(w) if w.enabled)
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
    fn send(_payload: &str) -> std::io::Result<bool> {
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
    fn heartbeat_enables_the_shared_xin_runtime_without_enabling_other_xin_work() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.modules.scheduler = true;
        config.xin.enabled = false;
        config.heartbeat.enabled = true;
        assert!(xin_runtime_enabled(&config));

        config.heartbeat.enabled = false;
        assert!(!xin_runtime_enabled(&config));

        config.xin.enabled = true;
        assert!(xin_runtime_enabled(&config));

        config.modules.scheduler = false;
        assert!(!xin_runtime_enabled(&config));
    }

    #[test]
    fn active_daemon_state_detects_recent_live_pid() {
        let now = Utc::now();
        let raw = serde_json::json!({
            "pid": std::process::id(),
            "written_at": now.to_rfc3339(),
            "components": {
                "daemon": {
                    "status": "ok",
                    "last_ok": now.to_rfc3339()
                }
            }
        })
        .to_string();

        assert_eq!(
            active_daemon_from_state_json(&raw, now),
            Some(ActiveDaemonState {
                pid: std::process::id()
            })
        );
    }

    #[test]
    fn active_daemon_state_ignores_stale_timestamp() {
        let now = Utc::now();
        let stale = now - chrono::Duration::seconds(MANUAL_DAEMON_STALE_SECONDS + 1);
        let raw = serde_json::json!({
            "pid": std::process::id(),
            "written_at": stale.to_rfc3339(),
            "components": {
                "daemon": {
                    "status": "ok",
                    "last_ok": stale.to_rfc3339()
                }
            }
        })
        .to_string();

        assert_eq!(active_daemon_from_state_json(&raw, now), None);
    }

    #[test]
    fn active_daemon_state_accepts_fresh_written_at_when_last_ok_is_stale() {
        let now = Utc::now();
        let stale = now - chrono::Duration::seconds(MANUAL_DAEMON_STALE_SECONDS + 1);
        let raw = serde_json::json!({
            "pid": std::process::id(),
            "written_at": now.to_rfc3339(),
            "components": {
                "daemon": {
                    "status": "ok",
                    "last_ok": stale.to_rfc3339()
                }
            }
        })
        .to_string();

        assert_eq!(
            active_daemon_from_state_json(&raw, now),
            Some(ActiveDaemonState {
                pid: std::process::id()
            })
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn active_daemon_state_ignores_dead_pid() {
        let now = Utc::now();
        let raw = serde_json::json!({
            "pid": u32::MAX,
            "written_at": now.to_rfc3339(),
            "components": {
                "daemon": {
                    "status": "ok",
                    "last_ok": now.to_rfc3339()
                }
            }
        })
        .to_string();

        assert_eq!(active_daemon_from_state_json(&raw, now), None);
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
        tokio::task::spawn_blocking(move || {
            receiver
                .recv_timeout(Duration::from_secs(2))
                .map_err(|error| error.to_string())?
        })
        .await
        .expect("ack task")
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
}
