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
    let latest = crate::self_system::fitness::latest_closed_window(Utc::now(), &config.self_system.fitness).unwrap();
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

    let root = resolve_evolution_storage_root(&config, &runtime).expect("test: resolve evolution root");
    assert_eq!(root, config.workspace_dir.join("self/evolution-data"));
}

#[test]
fn resolve_evolution_storage_root_preserves_absolute_path() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut runtime = EvolutionRuntimeConfig::default();
    let abs = tmp.path().join("absolute-storage");
    runtime.storage_dir = abs.to_string_lossy().to_string();

    let root = resolve_evolution_storage_root(&config, &runtime).expect("test: resolve evolution root");
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
    assert!(builder.contains("with_commit_tracker(crate::self_system::evolution::commit::daemon_commits().clone())"));
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
            crate::channels::start_channels_with_config((*generation.effective).clone(), shared, generation, shutdown)
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
        !crate::config::ConfigGenerationParticipant::supports_controlled_restart_field(&participant, "channels_config")
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
#[tokio::test]
async fn evolution_scheduler_is_disabled_without_the_skills_lifecycle() {
    let name = "daemon-test-evolution-gate";
    let calls = Arc::new(AtomicUsize::new(0));
    let runner: GenerationComponentRunner = {
        let calls = calls.clone();
        Arc::new(move |_, shutdown| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                crate::health::mark_component_ok(name);
                shutdown.cancelled().await;
                Ok(())
            })
        })
    };
    let enabled = evolution_scheduler_enable_predicate();
    let mut config = Config::default();
    config.skills_lifecycle.enabled = false;
    let generation = crate::config::new_shared(config.clone()).pin();
    let (handle, token) = start_generation_component(name, "test", 60, generation, &enabled, &runner)
        .await
        .unwrap();
    assert!(handle.is_none());
    assert!(token.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(crate::health::snapshot_json()["components"][name]["status"], "disabled");
    config.skills_lifecycle.enabled = true;
    config.skills_lifecycle.owner_scope = Some("owner:ws:cli:prx_user".into());
    let generation = crate::config::new_shared(config).pin();
    let (handle, token) = start_generation_component(name, "test", 60, generation, &enabled, &runner)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    token.unwrap().cancel();
    handle.unwrap().await.unwrap().unwrap();
}
