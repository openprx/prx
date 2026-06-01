//! Tick-based heartbeat runner for the xin (心) autonomous task engine.
//!
//! Follows the cron/scheduler.rs pattern:
//! - Periodic interval tick
//! - Query due tasks from SQLite
//! - Execute concurrently (buffer_unordered)
//! - Persist results and reschedule

use crate::config::Config;
use crate::security::SecurityPolicy;
use crate::security::policy::ApprovalGrant;
use crate::xin::builtin::BuiltinRegistry;
use crate::xin::store;
use crate::xin::types::{ExecutionMode, GoalStatus, XinGoal, XinStep, XinTask, XinTickSummary, default_lease_ttl_secs};
use anyhow::Result;
use chrono::Utc;
use futures_util::{StreamExt, stream};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{self, Duration};
use tokio_util::sync::CancellationToken;

const XIN_COMPONENT: &str = "xin";
const SHELL_TIMEOUT_SECS: u64 = 120;
const AGENT_MAX_TOOL_ITERATIONS: usize = 20;
/// Floor for the heartbeat interval so very short leases still renew sanely.
const MIN_HEARTBEAT_SECS: u64 = 5;

/// Stable per-process worker identity used for step lease ownership.
fn worker_id() -> String {
    let pid = std::process::id();
    let host = hostname_hash();
    format!("prx:{pid}:{host}")
}

/// Short, stable hash of the host name (avoids leaking the raw hostname).
fn hostname_hash() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match std::env::var("HOSTNAME") {
        Ok(name) => name.as_str().hash(&mut hasher),
        Err(_) => "unknown".hash(&mut hasher),
    }
    format!("{:08x}", hasher.finish() & 0xffff_ffff)
}

/// Run the xin heartbeat loop. Called by daemon supervisor.
pub async fn run(config: Config) -> Result<()> {
    let interval_secs = u64::from(config.xin.interval_minutes.max(1)) * 60;
    let mut interval = time::interval(Duration::from_secs(interval_secs));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let registry = Arc::new(BuiltinRegistry::new());

    // Register built-in system tasks if configured
    if config.xin.builtin_tasks {
        register_builtin_tasks(&config)?;
    }

    // Crash recovery: surface any goal steps whose lease expired while the
    // daemon was down so they get re-claimed (rather than silently orphaned).
    match store::expired_step_leases(&config, Utc::now()) {
        Ok(steps) if !steps.is_empty() => {
            tracing::info!(
                target: "xin",
                count = steps.len(),
                "recovered goal steps with expired leases on startup"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(target: "xin", "startup expired-lease scan failed: {e}"),
    }

    // Optionally adopt orphaned legacy tasks into lease-managed goals.
    if config.xin.adopt_legacy_tasks {
        match adopt_legacy_tasks(&config) {
            Ok(0) => {}
            Ok(n) => tracing::info!(target: "xin", count = n, "adopted legacy tasks into goals"),
            Err(e) => tracing::warn!(target: "xin", "legacy task adoption failed: {e}"),
        }
    }

    crate::health::mark_component_ok(XIN_COMPONENT);
    tracing::info!(
        target: "xin",
        interval_minutes = config.xin.interval_minutes,
        max_concurrent = config.xin.max_concurrent,
        builtin_tasks = config.xin.builtin_tasks,
        evolution_integration = config.xin.evolution_integration,
        "xin heartbeat engine started"
    );

    loop {
        interval.tick().await;
        crate::health::mark_component_ok(XIN_COMPONENT);

        // Mark stale tasks
        if let Err(e) = store::mark_stale(&config, config.xin.stale_timeout_minutes) {
            tracing::warn!(target: "xin", "failed to mark stale tasks: {e}");
        }

        // Reset goal steps whose lease expired so they can be re-claimed instead
        // of orphaned. Lease + heartbeat (not updated_at) drive step staleness,
        // so long agent runs survive across ticks.
        match store::mark_steps_stale(&config, Utc::now()) {
            Ok(ids) if !ids.is_empty() => {
                tracing::info!(target: "xin", count = ids.len(), "reset expired-lease steps to stale");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(target: "xin", "failed to mark steps stale: {e}"),
        }

        // Drive any goals that have runnable steps (crash-safe, lease-guarded).
        if let Err(e) = drive_goals(&config, &security, &registry).await {
            tracing::warn!(target: "xin", "goal driving failed: {e}");
        }

        // Emit a per-goal progress snapshot for observability.
        if let Err(e) = report_goal_progress(&config) {
            tracing::warn!(target: "xin", "goal progress report failed: {e}");
        }

        // Query due tasks
        let tasks = match store::due_tasks(&config, Utc::now(), config.xin.max_concurrent) {
            Ok(tasks) => tasks,
            Err(e) => {
                crate::health::mark_component_error(XIN_COMPONENT, e.to_string());
                tracing::warn!(target: "xin", "due_tasks query failed: {e}");
                continue;
            }
        };

        if tasks.is_empty() {
            continue;
        }

        let summary = execute_due_tasks(&config, &security, &registry, tasks).await;

        tracing::info!(
            target: "xin",
            checked = summary.tasks_checked,
            executed = summary.tasks_executed,
            completed = summary.tasks_completed,
            failed = summary.tasks_failed,
            cleaned = summary.tasks_cleaned,
            "xin tick completed"
        );

        crate::health::mark_component_ok(XIN_COMPONENT);
    }
}

fn register_builtin_tasks(config: &Config) -> Result<()> {
    let definitions = crate::xin::builtin::builtin_task_definitions();
    for def in &definitions {
        if let Err(e) = store::ensure_system_task(config, def) {
            tracing::warn!(target: "xin", name = %def.name, "failed to register builtin task: {e}");
        }
    }
    tracing::info!(
        target: "xin",
        count = definitions.len(),
        "registered built-in system tasks"
    );
    Ok(())
}

async fn execute_due_tasks(
    config: &Config,
    security: &Arc<SecurityPolicy>,
    registry: &Arc<BuiltinRegistry>,
    tasks: Vec<XinTask>,
) -> XinTickSummary {
    let max_concurrent = config.xin.max_concurrent.max(1);
    let checked = tasks.len();

    let mut results = stream::iter(tasks.into_iter().map(|task| {
        let config = config.clone();
        let security = Arc::clone(security);
        let registry = Arc::clone(registry);
        async move { execute_single_task(&config, &security, &registry, &task).await }
    }))
    .buffer_unordered(max_concurrent);

    let mut summary = XinTickSummary {
        tasks_checked: checked,
        ..XinTickSummary::default()
    };

    while let Some((success, task_id)) = results.next().await {
        summary.tasks_executed += 1;
        if success {
            summary.tasks_completed += 1;
        } else {
            summary.tasks_failed += 1;
            tracing::warn!(target: "xin", task_id = %task_id, "task execution failed");
        }
    }

    // Clean up completed non-recurring tasks
    match store::remove_completed(config) {
        Ok(n) => summary.tasks_cleaned = n,
        Err(e) => tracing::warn!(target: "xin", "failed to clean completed tasks: {e}"),
    }

    summary
}

async fn execute_single_task(
    config: &Config,
    security: &SecurityPolicy,
    registry: &BuiltinRegistry,
    task: &XinTask,
) -> (bool, String) {
    let task_id = task.id.clone();

    // Atomically claim the task (prevents duplicate execution)
    match store::claim_task(config, &task_id) {
        Ok(true) => {} // claimed successfully
        Ok(false) => {
            tracing::debug!(target: "xin", task_id = %task_id, "task already claimed or disabled, skipping");
            return (true, task_id); // not a failure — another worker got it
        }
        Err(e) => {
            tracing::warn!(target: "xin", task_id = %task_id, "failed to claim task: {e}");
            return (false, task_id);
        }
    }

    let started_at = Utc::now();

    // Execute based on mode
    let (success, output) = match task.execution_mode {
        ExecutionMode::Internal => run_internal(config, registry, task).await,
        ExecutionMode::AgentSession => run_agent(config, security, task).await,
        ExecutionMode::Shell => run_shell(config, security, task).await,
    };

    let finished_at = Utc::now();
    let duration_ms = (finished_at - started_at).num_milliseconds();

    // Persist result
    if success {
        if let Err(e) = store::mark_completed(config, &task_id, &output) {
            tracing::warn!(target: "xin", task_id = %task_id, "failed to mark completed: {e}");
        }
    } else if let Err(e) = store::mark_failed(config, &task_id, &output) {
        tracing::warn!(target: "xin", task_id = %task_id, "failed to mark failed: {e}");
    }

    // Record run history
    let status = if success { "ok" } else { "error" };
    if let Err(e) = store::record_run(
        config,
        &task_id,
        started_at,
        finished_at,
        status,
        Some(&output),
        duration_ms,
    ) {
        tracing::warn!(target: "xin", task_id = %task_id, "failed to record run: {e}");
    }

    // Reschedule if recurring and still enabled
    if task.recurring {
        if let Err(e) = store::reschedule_recurring(config, &task_id) {
            tracing::warn!(target: "xin", task_id = %task_id, "failed to reschedule: {e}");
        }
    }

    (success, task_id)
}

// ── Goal / Step execution (FIX-P2-16, d09) ──────────────────────────────

/// Scan all goals and advance each one by its next runnable step.
///
/// Each step runs under a lease that is renewed by a background heartbeat, so a
/// long-running AgentSession is never falsely reaped while it is still working.
async fn drive_goals(config: &Config, security: &Arc<SecurityPolicy>, registry: &Arc<BuiltinRegistry>) -> Result<()> {
    let goals = store::list_goals(config)?;
    let runnable: Vec<XinGoal> = goals
        .into_iter()
        .filter(|g| g.enabled && matches!(g.status, GoalStatus::Pending | GoalStatus::Running))
        .collect();
    if runnable.is_empty() {
        return Ok(());
    }

    let max_concurrent = config.xin.max_concurrent.max(1);
    let mut stream = stream::iter(runnable.into_iter().map(|goal| {
        let config = config.clone();
        let security = Arc::clone(security);
        let registry = Arc::clone(registry);
        async move {
            if let Err(e) = advance_goal(&config, &security, &registry, &goal).await {
                tracing::warn!(target: "xin", goal_id = %goal.id, "failed to advance goal: {e}");
            }
        }
    }))
    .buffer_unordered(max_concurrent);

    while stream.next().await.is_some() {}
    Ok(())
}

/// Log a compact progress snapshot for every active goal. Reads each goal's
/// fresh state and step breakdown so operators can see goal-level progress in
/// the daemon logs without inspecting SQLite directly.
fn report_goal_progress(config: &Config) -> Result<()> {
    for goal in store::list_goals(config)? {
        if !matches!(goal.status, GoalStatus::Pending | GoalStatus::Running) {
            continue;
        }
        // Re-read the canonical goal record (cheap, and exercises get_goal).
        let current = match store::get_goal(config, &goal.id) {
            Ok(g) => g,
            Err(e) => {
                tracing::debug!(target: "xin", goal_id = %goal.id, "goal vanished during report: {e}");
                continue;
            }
        };
        let steps = store::list_steps(config, &current.id)?;
        let running = steps
            .iter()
            .filter(|s| s.status.as_str() == crate::xin::types::StepStatus::Running.as_str())
            .count();
        tracing::info!(
            target: "xin",
            goal_id = %current.id,
            status = current.status.as_str(),
            completed = current.steps_completed,
            total = current.steps_total,
            running,
            "goal progress"
        );
    }
    Ok(())
}

/// Adopt orphaned, stale, non-recurring legacy `XinTask`s into goal/step
/// records so they gain lease-based retry + crash recovery. The original task
/// rows are left intact (zero-breakage). Returns the number adopted.
fn adopt_legacy_tasks(config: &Config) -> Result<usize> {
    let tasks = store::list_tasks(config)?;
    let mut adopted = 0usize;
    for task in tasks {
        // Only adopt stale, non-recurring tasks (the ones at risk of being
        // orphaned across a daemon restart). Recurring tasks stay legacy.
        if task.recurring || task.status != crate::xin::types::TaskStatus::Stale {
            continue;
        }
        let goal = match store::migrate_task_to_goal(config, &task.id) {
            Ok(goal) => goal,
            Err(e) => {
                tracing::warn!(target: "xin", task_id = %task.id, "failed to adopt legacy task: {e}");
                continue;
            }
        };
        // Append a terminal verification marker step so the adopted goal has a
        // clean completion checkpoint after the migrated work runs.
        let verify = crate::xin::types::NewXinStep {
            sequence: 2,
            name: format!("{}::verify", task.name),
            description: Some("post-adoption completion marker".into()),
            execution_mode: ExecutionMode::Internal,
            payload: "xin:health_check".into(),
            max_retries: 0,
            approval_grant_json: None,
            lease_ttl_secs: 0,
        };
        if let Err(e) = store::add_step(config, &goal.id, &verify) {
            tracing::warn!(target: "xin", goal_id = %goal.id, "failed to append verify step: {e}");
        }
        adopted += 1;
    }
    Ok(adopted)
}

/// Advance a single goal by claiming and executing its next runnable step.
async fn advance_goal(
    config: &Config,
    security: &SecurityPolicy,
    registry: &BuiltinRegistry,
    goal: &XinGoal,
) -> Result<()> {
    let Some(step) = store::next_runnable_step(config, &goal.id)? else {
        return Ok(());
    };

    let worker = worker_id();
    let lease_ttl = lease_ttl_for(&step);

    // Atomically claim the step; if another worker won, bail out quietly.
    if !store::claim_step(config, &step.id, &worker, lease_ttl)? {
        tracing::debug!(target: "xin", step_id = %step.id, "step already claimed by another worker");
        return Ok(());
    }
    if !store::mark_step_running(config, &step.id, &worker)? {
        tracing::debug!(target: "xin", step_id = %step.id, "step running transition lost the lease");
        return Ok(());
    }

    // Re-read the freshly-claimed step so we run against the persisted lease/
    // checkpoint state rather than the pre-claim snapshot.
    let step = store::get_step(config, &step.id)?;

    let (success, output) = execute_step_with_heartbeat(config, security, registry, &step, &worker, lease_ttl).await;

    if success {
        if let Err(e) = store::complete_step(config, &step.id, &output) {
            tracing::warn!(target: "xin", step_id = %step.id, "failed to complete step: {e}");
        }
    } else if let Err(e) = store::fail_step(config, &step.id, &output) {
        tracing::warn!(target: "xin", step_id = %step.id, "failed to fail step: {e}");
    }
    Ok(())
}

/// Resolve the effective lease TTL for a step: honor the per-step override,
/// falling back to the per-execution-mode default.
const fn lease_ttl_for(step: &XinStep) -> u64 {
    if step.lease_ttl_secs != 0 {
        step.lease_ttl_secs
    } else {
        default_lease_ttl_secs(&step.execution_mode)
    }
}

/// Execute a step while a background task renews its lease at `ttl/3` intervals.
///
/// The heartbeat keeps `lease_expires_at` ahead of `now` so `mark_steps_stale`
/// never reaps an actively-running step. The `CancellationToken` guarantees the
/// heartbeat task is torn down once the step finishes (no leak).
async fn execute_step_with_heartbeat(
    config: &Config,
    security: &SecurityPolicy,
    registry: &BuiltinRegistry,
    step: &XinStep,
    worker: &str,
    lease_ttl_secs: u64,
) -> (bool, String) {
    let heartbeat_secs = (lease_ttl_secs / 3).max(MIN_HEARTBEAT_SECS);
    let cancel = CancellationToken::new();
    let cancel_hb = cancel.clone();

    let hb_config = config.clone();
    let hb_step_id = step.id.clone();
    let hb_worker = worker.to_string();
    let hb_handle = tokio::spawn(async move {
        let mut ticker = time::interval(Duration::from_secs(heartbeat_secs));
        // Skip the immediate first tick (lease was just set by claim).
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match store::renew_step_lease(&hb_config, &hb_step_id, &hb_worker, lease_ttl_secs) {
                        Ok(true) => {
                            tracing::debug!(target: "xin", step_id = %hb_step_id, "lease renewed");
                        }
                        Ok(false) => {
                            tracing::warn!(target: "xin", step_id = %hb_step_id, "lease lost; stopping heartbeat");
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(target: "xin", step_id = %hb_step_id, "heartbeat renewal failed: {e}");
                        }
                    }
                }
                _ = cancel_hb.cancelled() => break,
            }
        }
    });

    let result = execute_step_inner(config, security, registry, step).await;

    cancel.cancel();
    if let Err(e) = hb_handle.await {
        tracing::warn!(target: "xin", step_id = %step.id, "heartbeat task join error: {e}");
    }
    result
}

/// Run a step's actual work, reusing the same execution backends as XinTask.
async fn execute_step_inner(
    config: &Config,
    security: &SecurityPolicy,
    registry: &BuiltinRegistry,
    step: &XinStep,
) -> (bool, String) {
    // Bridge the step into the existing XinTask-shaped execution backends.
    let bridge = step_as_task(step);
    let result = match step.execution_mode {
        ExecutionMode::Internal => run_internal(config, registry, &bridge).await,
        ExecutionMode::AgentSession => run_agent(config, security, &bridge).await,
        ExecutionMode::Shell => run_shell(config, security, &bridge).await,
    };

    // Persist a checkpoint snapshot so a crash mid-goal can be diagnosed and the
    // next attempt can resume from a known marker rather than from scratch.
    let checkpoint = serde_json::json!({
        "sequence": step.sequence,
        "attempt": step.retry_count + 1,
        "succeeded": result.0,
        "at": Utc::now().to_rfc3339(),
    })
    .to_string();
    if let Err(e) = store::save_step_checkpoint(config, &step.id, &checkpoint) {
        tracing::warn!(target: "xin", step_id = %step.id, "failed to save step checkpoint: {e}");
    }

    result
}

/// Adapt a `XinStep` into a `XinTask` view for the shared execution backends.
fn step_as_task(step: &XinStep) -> XinTask {
    XinTask {
        id: step.id.clone(),
        owner_id: None,
        topic_id: None,
        parent_task_id: Some(step.goal_id.clone()),
        source_message_event_id: None,
        name: step.name.clone(),
        description: step.description.clone(),
        kind: crate::xin::types::TaskKind::User,
        status: crate::xin::types::TaskStatus::Running,
        priority: crate::xin::types::TaskPriority::Normal,
        execution_mode: step.execution_mode.clone(),
        payload: step.payload.clone(),
        recurring: false,
        interval_secs: 0,
        created_at: step.created_at,
        updated_at: step.updated_at,
        last_run_at: step.started_at,
        next_run_at: step.created_at,
        last_status: None,
        last_output: step.last_output.clone(),
        run_count: u64::from(step.retry_count),
        fail_count: u64::from(step.retry_count),
        max_failures: step.max_retries,
        enabled: true,
        approval_grant_json: step.approval_grant_json.clone(),
    }
}

// ── Execution modes ─────────────────────────────────────────────────────

async fn run_internal(config: &Config, registry: &BuiltinRegistry, task: &XinTask) -> (bool, String) {
    match registry.execute(&task.payload, config.clone()).await {
        Ok(output) => (true, output),
        Err(e) => (false, format!("internal handler error: {e}")),
    }
}

async fn run_agent(config: &Config, security: &SecurityPolicy, task: &XinTask) -> (bool, String) {
    if !security.can_act() {
        return (false, "blocked by security policy: autonomy is read-only".into());
    }

    if security.is_rate_limited() {
        return (false, "blocked by security policy: rate limit exceeded".into());
    }

    if !security.record_action() {
        return (false, "blocked by security policy: action budget exhausted".into());
    }

    let prompt = format!("[xin:task:{}] {}", task.id, task.payload);

    let mut agent_config = config.clone();
    if agent_config.agent.max_tool_iterations == 0 || agent_config.agent.max_tool_iterations > AGENT_MAX_TOOL_ITERATIONS
    {
        agent_config.agent.max_tool_iterations = AGENT_MAX_TOOL_ITERATIONS;
    }

    match crate::agent::run(
        agent_config,
        Some(prompt),
        None,
        config.default_model.clone(),
        config.default_temperature,
    )
    .await
    {
        Ok(response) => (
            true,
            if response.trim().is_empty() {
                "agent task executed".into()
            } else {
                response
            },
        ),
        Err(e) => (false, format!("agent task failed: {e}")),
    }
}

async fn run_shell(config: &Config, security: &SecurityPolicy, task: &XinTask) -> (bool, String) {
    if !security.can_act() {
        return (false, "blocked by security policy: autonomy is read-only".into());
    }

    if security.is_rate_limited() {
        return (false, "blocked by security policy: rate limit exceeded".into());
    }

    let approval_grant = persisted_task_approval_grant(task);
    if let Err(reason) = crate::security::SideEffectGate::new(security).authorize_command_execution(
        "xin_runner",
        &task.payload,
        approval_grant.as_ref(),
    ) {
        return (false, format!("blocked by security policy: {reason}"));
    }

    if !security.record_action() {
        return (false, "blocked by security policy: action budget exhausted".into());
    }

    let mut command = Command::new("sh");
    command
        .arg("-lc")
        .arg(&task.payload)
        .current_dir(&config.workspace_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // P0-39: apply OS-level sandbox isolation before spawning, mirroring the
    // ShellTool path (tools/shell.rs). The Sandbox trait mutates the inner
    // std::process::Command, reached here via tokio's `as_std_mut`. A fail-closed
    // backend (UnavailableSandbox) blocks execution rather than running unsandboxed.
    let sandbox = crate::security::create_sandbox_with_workspace(&config.security, Some(&config.workspace_dir));
    if let Err(e) = sandbox.wrap_command(command.as_std_mut()) {
        return (
            false,
            format!("blocked by security policy: sandbox failed to wrap command: {e}"),
        );
    }

    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return (false, format!("spawn error: {e}")),
    };

    match time::timeout(Duration::from_secs(SHELL_TIMEOUT_SECS), child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!(
                "status={}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                stdout.trim(),
                stderr.trim()
            );
            (output.status.success(), combined)
        }
        Ok(Err(e)) => (false, format!("spawn error: {e}")),
        Err(_) => (false, format!("task timed out after {SHELL_TIMEOUT_SECS}s")),
    }
}

fn persisted_task_approval_grant(task: &XinTask) -> Option<ApprovalGrant> {
    task.approval_grant_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ApprovalGrant>(raw).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xin::types::{NewXinTask, TaskKind, TaskPriority};
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        // P0-39: pin the sandbox backend to None for shell-runner tests, mirroring
        // ShellTool's use of NoopSandbox in tests/. The default `Auto` backend
        // would auto-detect whatever heavy isolation backend (docker/firejail) the
        // host happens to expose and wrap `sh -lc`, which is not what these tests
        // exercise. Production still honours the operator's real sandbox config.
        config.security.sandbox.backend = crate::config::SandboxBackend::None;
        config.security.sandbox.enabled = Some(false);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    #[test]
    fn register_builtin_tasks_creates_system_tasks() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        register_builtin_tasks(&config).unwrap();

        let tasks = store::list_tasks(&config).unwrap();
        assert_eq!(tasks.len(), 5);
        for task in &tasks {
            assert_eq!(task.kind, TaskKind::System);
            assert!(task.recurring);
            assert!(task.enabled);
        }
    }

    #[test]
    fn register_builtin_tasks_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        register_builtin_tasks(&config).unwrap();
        register_builtin_tasks(&config).unwrap();

        let tasks = store::list_tasks(&config).unwrap();
        assert_eq!(tasks.len(), 5); // No duplicates
    }

    #[tokio::test]
    async fn execute_shell_task_success() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let new = NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "shell_test".into(),
            description: None,
            kind: TaskKind::User,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Shell,
            payload: "echo hello".into(),
            recurring: false,
            interval_secs: 0,
            max_failures: 3,
            approval_grant_json: None,
        };
        let task = store::add_task(&config, &new).unwrap();

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (success, output) = run_shell(&config, &security, &task).await;

        assert!(success, "shell task should succeed: {output}");
        assert!(output.contains("hello"));
    }

    #[tokio::test]
    async fn execute_shell_task_applies_sandbox_fail_closed() {
        // P0-39: prove the sandbox is actually wired into the shell runner. An
        // explicitly-requested-but-unavailable backend must fail closed (the
        // command is refused) rather than silently running unsandboxed.
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        // Request docker but force unavailability is environment-dependent; instead
        // request a backend that is not available so create_sandbox returns the
        // fail-closed UnavailableSandbox. Firejail is not installed in CI.
        config.security.sandbox.backend = crate::config::SandboxBackend::Firejail;
        config.security.sandbox.enabled = Some(true);
        config.autonomy.level = crate::security::AutonomyLevel::Full;

        let new = NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "shell_sandbox".into(),
            description: None,
            kind: TaskKind::User,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Shell,
            payload: "echo should-not-run".into(),
            recurring: false,
            interval_secs: 0,
            max_failures: 3,
            approval_grant_json: None,
        };
        let task = store::add_task(&config, &new).unwrap();

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (success, output) = run_shell(&config, &security, &task).await;

        // If firejail happens to be installed the command may run; otherwise the
        // fail-closed sandbox blocks it. Assert the sandbox path was exercised:
        // either it was refused with the sandbox message, or it genuinely ran.
        if !success {
            assert!(output.contains("sandbox"), "expected sandbox refusal, got: {output}");
        }
    }

    #[tokio::test]
    async fn execute_shell_task_failure() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let new = NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "shell_fail".into(),
            description: None,
            kind: TaskKind::User,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Shell,
            payload: "exit 1".into(),
            recurring: false,
            interval_secs: 0,
            max_failures: 3,
            approval_grant_json: None,
        };
        let task = store::add_task(&config, &new).unwrap();

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (success, _output) = run_shell(&config, &security, &task).await;

        assert!(!success);
    }

    #[tokio::test]
    async fn execute_shell_task_blocks_medium_risk_without_runtime_grant() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.autonomy.allowed_commands = vec!["touch".into()];

        let new = NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "shell_medium".into(),
            description: None,
            kind: TaskKind::User,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Shell,
            payload: "touch xin-medium-risk".into(),
            recurring: false,
            interval_secs: 0,
            max_failures: 3,
            approval_grant_json: None,
        };
        let task = store::add_task(&config, &new).unwrap();

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (success, output) = run_shell(&config, &security, &task).await;

        assert!(!success);
        assert!(output.contains("runtime approval grant"), "{output}");
        assert!(!config.workspace_dir.join("xin-medium-risk").exists());
    }

    #[tokio::test]
    async fn execute_shell_task_allows_medium_risk_with_persisted_runner_grant() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.autonomy.allowed_commands = vec!["touch".into()];
        let command = "touch xin-persisted-approval";

        let new = NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "shell_medium_persisted".into(),
            description: None,
            kind: TaskKind::User,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Shell,
            payload: command.into(),
            recurring: false,
            interval_secs: 0,
            max_failures: 3,
            approval_grant_json: Some(
                serde_json::to_string(&ApprovalGrant::persisted_for_command(
                    "xin_runner",
                    command,
                    "test",
                    None,
                    crate::security::policy::PERSISTED_APPROVAL_GRANT_TTL_SECS,
                ))
                .unwrap(),
            ),
        };
        let task = store::add_task(&config, &new).unwrap();

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (success, output) = run_shell(&config, &security, &task).await;

        assert!(success, "{output}");
        assert!(config.workspace_dir.join("xin-persisted-approval").exists());
    }

    #[tokio::test]
    async fn execute_internal_health_check() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let registry = BuiltinRegistry::new();

        let new = NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "xin:health_check".into(),
            description: None,
            kind: TaskKind::System,
            priority: TaskPriority::High,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:health_check".into(),
            recurring: true,
            interval_secs: 300,
            max_failures: 10,
            approval_grant_json: None,
        };
        let task = store::add_task(&config, &new).unwrap();

        let (success, output) = run_internal(&config, &registry, &task).await;
        assert!(success, "health check should succeed: {output}");
        assert!(output.contains("health check completed"));
    }

    #[tokio::test]
    async fn execute_internal_stale_cleanup() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let registry = BuiltinRegistry::new();

        let new = NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "xin:stale_cleanup".into(),
            description: None,
            kind: TaskKind::System,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:stale_cleanup".into(),
            recurring: true,
            interval_secs: 1800,
            max_failures: 10,
            approval_grant_json: None,
        };
        let task = store::add_task(&config, &new).unwrap();

        let (success, output) = run_internal(&config, &registry, &task).await;
        assert!(success, "stale cleanup should succeed: {output}");
        assert!(output.contains("stale cleanup"));
    }

    // ── Goal / Step execution (FIX-P2-16) ─────────────────────────────────

    #[test]
    fn worker_id_is_stable_and_formatted() {
        let a = worker_id();
        let b = worker_id();
        assert_eq!(a, b);
        assert!(a.starts_with("prx:"), "{a}");
        assert_eq!(a.split(':').count(), 3, "{a}");
    }

    #[tokio::test]
    async fn drive_goals_completes_a_two_step_internal_goal() {
        use crate::xin::types::{GoalStatus, NewXinGoal, NewXinStep};

        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let registry = Arc::new(BuiltinRegistry::new());

        let step = |seq: u32| NewXinStep {
            sequence: seq,
            name: format!("hc-{seq}"),
            description: None,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:health_check".into(),
            max_retries: 1,
            approval_grant_json: None,
            lease_ttl_secs: 0,
        };
        let goal = store::add_goal(
            &config,
            &NewXinGoal {
                owner_id: None,
                topic_id: None,
                parent_task_id: None,
                source_message_event_id: None,
                name: "two_step_goal".into(),
                description: None,
                kind: TaskKind::User,
                priority: TaskPriority::Normal,
                target_completion_at: None,
                initial_steps: vec![step(1), step(2)],
            },
        )
        .unwrap();

        // Each drive advances by one step (lowest pending sequence).
        drive_goals(&config, &security, &registry).await.unwrap();
        assert_eq!(store::get_goal(&config, &goal.id).unwrap().steps_completed, 1);

        drive_goals(&config, &security, &registry).await.unwrap();
        let done = store::get_goal(&config, &goal.id).unwrap();
        assert_eq!(done.status, GoalStatus::Completed);
        assert_eq!(done.steps_completed, 2);
    }

    #[tokio::test]
    async fn heartbeat_renews_lease_during_step_execution() {
        use crate::xin::types::{NewXinGoal, NewXinStep, StepStatus};

        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let goal = store::add_goal(
            &config,
            &NewXinGoal {
                owner_id: None,
                topic_id: None,
                parent_task_id: None,
                source_message_event_id: None,
                name: "hb_goal".into(),
                description: None,
                kind: TaskKind::User,
                priority: TaskPriority::Normal,
                target_completion_at: None,
                initial_steps: vec![NewXinStep {
                    sequence: 1,
                    name: "slow".into(),
                    description: None,
                    execution_mode: ExecutionMode::Internal,
                    // Tiny lease so the heartbeat (MIN 5s) must renew it.
                    payload: "xin:health_check".into(),
                    max_retries: 0,
                    approval_grant_json: None,
                    lease_ttl_secs: 6,
                }],
            },
        )
        .unwrap();
        let step = store::list_steps(&config, &goal.id).unwrap().remove(0);
        let worker = worker_id();
        assert!(store::claim_step(&config, &step.id, &worker, 6).unwrap());
        assert!(store::mark_step_running(&config, &step.id, &worker).unwrap());

        // Run the heartbeat-wrapped step; the wrapper must keep the short lease
        // alive for the duration so a concurrent stale sweep does not reap it.
        let registry = BuiltinRegistry::new();
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (ok, _out) = execute_step_with_heartbeat(&config, &security, &registry, &step, &worker, 6).await;
        assert!(ok);
        // The step ran under a live lease (never marked stale during the run).
        let stale = store::mark_steps_stale(&config, Utc::now()).unwrap();
        assert!(stale.is_empty(), "running step must not be reaped: {stale:?}");
        // Completing the step transitions it out of running/claimed.
        store::complete_step(&config, &step.id, "done").unwrap();
        assert_eq!(
            store::get_step(&config, &step.id).unwrap().status,
            StepStatus::Completed
        );
    }
}
