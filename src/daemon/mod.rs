#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::config::{Config, HotReloadManager, new_shared};
use crate::self_system::evolution::{
    AsyncJsonlWriter, EvolutionAnalyzer, EvolutionConfig, EvolutionPipeline, EvolutionRetentionConfig,
    EvolutionRuntimeConfig, EvolutionScheduler, JsonlRetentionPolicy, JsonlStoragePaths, MemoryEvolutionEngine,
    PromptEvolutionEngine, StrategyEvolutionEngine, new_shared_evolution_config,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Timelike, Utc};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

const STATUS_FLUSH_SECONDS: u64 = 5;
const MANUAL_DAEMON_STALE_SECONDS: i64 = 30;

pub async fn run(config: Config, host: String, port: u16, shutdown: CancellationToken) -> Result<()> {
    ensure_manual_daemon_start_allowed(&config)?;

    let initial_backoff = config.reliability.channel_initial_backoff_secs.max(1);
    let max_backoff = config.reliability.channel_max_backoff_secs.max(initial_backoff);

    // Activate hot-reload watcher so config.toml changes take effect without restart.
    // D2: the watcher and the gateway must observe the SAME SharedConfig snapshot so
    // file-driven reloads are visible at every gateway authorization point. Build one
    // handle here, hand it to the watcher AND to the gateway supervisor below.
    let shared_config = new_shared(config.clone());
    let _hot_reload = {
        let config_path = config.config_path.clone();
        HotReloadManager::spawn(config_path, Arc::clone(&shared_config))
    };

    crate::health::mark_component_ok("daemon");
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

    let mut handles: Vec<JoinHandle<()>> = vec![spawn_state_writer(config.clone())];

    // Gateway always starts — modules.network only controls whether network.toml
    // is loaded, not the gateway itself.
    {
        let gateway_cfg = config.clone();
        let gateway_host = host.clone();
        let gateway_shared = Arc::clone(&shared_config);
        handles.push(spawn_component_supervisor(
            "gateway",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = gateway_cfg.clone();
                let host = gateway_host.clone();
                let shared = Arc::clone(&gateway_shared);
                async move {
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

    if config.modules.channels {
        if has_supervised_channels(&config) {
            let channels_cfg = config.clone();
            handles.push(spawn_component_supervisor(
                "channels",
                initial_backoff,
                max_backoff,
                move || {
                    let cfg = channels_cfg.clone();
                    // Supervised channels: the supervisor restarts on exit, so
                    // it uses a never-cancelled token. See never_cancelled_shutdown.
                    async move {
                        crate::channels::start_channels(cfg, crate::runtime::shutdown::never_cancelled_shutdown()).await
                    }
                },
            ));
        } else {
            crate::health::mark_component_ok("channels");
            tracing::info!("No real-time channels configured; channel supervisor disabled");
        }
    } else {
        crate::health::mark_component_ok("channels");
        tracing::debug!("Channels module disabled, skipping channel supervisor startup");
    }

    if config.modules.scheduler && config.heartbeat.enabled {
        let heartbeat_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "heartbeat",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = heartbeat_cfg.clone();
                async move { run_heartbeat_worker(cfg).await }
            },
        ));
    } else if !config.modules.scheduler {
        crate::health::mark_component_ok("heartbeat");
        tracing::debug!("Scheduler module disabled, skipping heartbeat startup");
    }

    if config.modules.scheduler && config.cron.enabled {
        let scheduler_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "scheduler",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = scheduler_cfg.clone();
                async move { crate::cron::scheduler::run(cfg).await }
            },
        ));
    } else {
        crate::health::mark_component_ok("scheduler");
        if !config.modules.scheduler {
            tracing::debug!("Scheduler module disabled, skipping cron startup");
        } else {
            tracing::info!("Cron disabled; scheduler supervisor not started");
        }
    }

    // ── Xin (心) autonomous task engine ──
    if config.modules.scheduler && config.xin.enabled {
        let xin_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "xin",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = xin_cfg.clone();
                async move { crate::xin::runner::run(cfg).await }
            },
        ));
    } else {
        crate::health::mark_component_ok("xin");
        if !config.modules.scheduler {
            tracing::debug!("Scheduler module disabled, skipping xin startup");
        } else {
            tracing::info!("Xin disabled; xin supervisor not started");
        }
    }

    // ── Self-system fitness ──
    // Xin takes over fitness only when ALL three flags are true:
    // xin.enabled + xin.builtin_tasks + xin.evolution_integration.
    // If builtin_tasks is false, xin won't register the fitness task, so
    // the standalone worker must still run.
    // Note: xin_manages_evolution requires scheduler module for xin to run.
    let xin_manages_evolution =
        config.modules.scheduler && config.xin.enabled && config.xin.builtin_tasks && config.xin.evolution_integration;
    let spawn_fitness = config.self_system.enabled && !xin_manages_evolution;
    if spawn_fitness {
        let fitness_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "self_system_fitness",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = fitness_cfg.clone();
                async move { run_fitness_worker(cfg).await }
            },
        ));
    } else {
        crate::health::mark_component_ok("self_system_fitness");
        if xin_manages_evolution {
            tracing::info!("Fitness managed by xin; standalone fitness supervisor not started");
        } else {
            tracing::info!("Self-system fitness disabled; fitness supervisor not started");
        }
    }

    // ── Evolution scheduler ──
    // Same guard: xin takes over evolution only when all three flags are on.
    let spawn_evolution = config.self_system.evolution_enabled && !xin_manages_evolution;
    if spawn_evolution {
        let evolution_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "evolution_scheduler",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = evolution_cfg.clone();
                async move { run_evolution_scheduler_worker(cfg).await }
            },
        ));
    } else {
        crate::health::mark_component_ok("evolution_scheduler");
        if xin_manages_evolution {
            tracing::info!("Evolution managed by xin; standalone evolution supervisor not started");
        } else {
            tracing::info!("Evolution scheduler disabled; evolution supervisor not started");
        }
    }

    if config.modules.integrations && config.webhook.enabled {
        let webhook_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "webhook_receiver",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = webhook_cfg.clone();
                async move {
                    let token =
                        cfg.webhook.token.as_deref().ok_or_else(|| {
                            anyhow::anyhow!("webhook.token must be configured when webhook.enabled=true")
                        })?;
                    // FIX-P1-03: pass the security policy so the standalone webhook
                    // server gates topic-store persistence on autonomy (ReadOnly = no write).
                    let webhook_security = crate::runtime::bootstrap::build_security_policy(&cfg);
                    crate::webhook::run(
                        &cfg.webhook.bind,
                        token,
                        &cfg.workspace_dir,
                        cfg.memory.acl_enabled,
                        webhook_security,
                    )
                    .await
                }
            },
        ));
    } else {
        crate::health::mark_component_ok("webhook_receiver");
        if !config.modules.integrations {
            tracing::debug!("Integrations module disabled, skipping webhook receiver startup");
        } else {
            tracing::info!("Webhook receiver disabled; webhook supervisor not started");
        }
    }

    println!("🧠 OpenPRX daemon started");
    println!("   Gateway:  http://{host}:{port}");
    println!(
        "   Components: gateway, channels, heartbeat, scheduler, xin, self_system_fitness, evolution_scheduler, webhook_receiver"
    );
    println!("   Ctrl+C to stop");
    systemd_notify::ready();

    // D5/D9 step 4: wait for either the external root shutdown token (e.g. the
    // dispatch-owned signal task wired in A6) or a direct ctrl_c as a fallback.
    // The abort-based teardown below is preserved verbatim: the daemon performs
    // a stateless exit and must not be turned into a graceful child-await.
    tokio::select! {
        () = shutdown.cancelled() => {}
        res = tokio::signal::ctrl_c() => res?,
    }
    crate::health::mark_component_error("daemon", "shutdown requested");
    systemd_notify::stopping();

    for handle in &handles {
        handle.abort();
    }
    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

pub fn state_file_path(config: &Config) -> PathBuf {
    config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("daemon_state.json")
}

fn spawn_state_writer(config: Config) -> JoinHandle<()> {
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
            crate::health::mark_component_ok("daemon");
            let mut json = crate::health::snapshot_json();
            if let Some(obj) = json.as_object_mut() {
                obj.insert("written_at".into(), serde_json::json!(Utc::now().to_rfc3339()));
            }
            let data = serde_json::to_vec_pretty(&json).unwrap_or_else(|_| b"{}".to_vec());
            if let Err(e) = tokio::fs::write(&path, &data).await {
                tracing::warn!("Failed to write daemon state file: {e}");
            }
            systemd_notify::watchdog();
        }
    })
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
            crate::health::mark_component_ok(name);
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

async fn run_heartbeat_worker(config: Config) -> Result<()> {
    let observer: std::sync::Arc<dyn crate::observability::Observer> =
        std::sync::Arc::from(crate::observability::create_observer(&config.observability));
    let engine = crate::heartbeat::engine::HeartbeatEngine::new(
        config.heartbeat.clone(),
        config.workspace_dir.clone(),
        observer,
    );

    let interval_mins = config.heartbeat.interval_minutes.max(5);
    let mut interval = tokio::time::interval(Duration::from_secs(u64::from(interval_mins) * 60));

    loop {
        interval.tick().await;
        let local_hour = chrono::Local::now().hour() as u8;
        if !crate::heartbeat::engine::HeartbeatEngine::is_within_active_hours(&config.heartbeat, local_hour) {
            continue;
        }

        let prompts = engine.collect_task_prompts().await?;
        if prompts.is_empty() {
            continue;
        }

        for prompt in prompts {
            let temp = config.default_temperature;
            // Background heartbeat: no cooperative shutdown signal of its own;
            // the supervisor aborts the task. See never_cancelled_shutdown docs.
            if let Err(e) = crate::agent::run(
                config.clone(),
                Some(prompt),
                None,
                None,
                temp,
                crate::runtime::shutdown::never_cancelled_shutdown(),
            )
            .await
            {
                crate::health::mark_component_error("heartbeat", e.to_string());
                tracing::warn!("Heartbeat task failed: {e}");
            } else {
                crate::health::mark_component_ok("heartbeat");
            }
        }
    }
}

async fn run_fitness_worker(config: Config) -> Result<()> {
    let interval_hours = config.self_system.fitness_interval_hours.max(1);
    let mut interval = tokio::time::interval(Duration::from_secs(interval_hours.saturating_mul(3600)));

    loop {
        interval.tick().await;
        match crate::self_system::run_fitness_report().await {
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

async fn run_evolution_scheduler_worker(config: Config) -> Result<()> {
    let (mut scheduler, interval_hours) = build_evolution_scheduler(&config).await?;
    let mut interval = tokio::time::interval(Duration::from_secs(interval_hours.max(1).saturating_mul(3600)));

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

    let memory_engine = Box::new(
        MemoryEvolutionEngine::new(shared.clone(), &cfg_path, Some(writer.clone()))
            .with_context(|| format!("failed to initialize memory evolution engine: {}", cfg_path.display()))?,
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
    let provider_runtime_options = crate::providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        openprx_dir: config.config_path.parent().map(PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        codex_auth_json_path: Some(config.auth.codex_auth_json_path.clone()),
        codex_auth_json_auto_import: config.auth.codex_auth_json_auto_import,
        reasoning_enabled: config.runtime.reasoning_enabled,
        codex_stream_idle_timeout_secs: config.runtime.codex_stream_idle_timeout_secs,
        codex_reasoning_effort: config.runtime.codex_reasoning_effort.clone(),
    };

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
