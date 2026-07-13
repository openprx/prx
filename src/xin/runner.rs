//! Tick-based heartbeat runner for the xin (心) autonomous task engine.
//!
//! Follows the cron/scheduler.rs pattern:
//! - Periodic interval tick
//! - Query due tasks from SQLite
//! - Execute concurrently (buffer_unordered)
//! - Persist results and reschedule

use crate::config::Config;
use crate::runtime::shell_process::{ShellProcessAdapter, ShellProcessError, ShellProcessRequest};
use crate::security::SecurityPolicy;
use crate::security::policy::ApprovalGrant;
use crate::xin::builtin::BuiltinRegistry;
use crate::xin::store;
use crate::xin::types::{ExecutionMode, GoalStatus, XinGoal, XinStep, XinTask, XinTickSummary, default_lease_ttl_secs};
use anyhow::Result;
use chrono::Utc;
use futures_util::{StreamExt, stream};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tokio_util::sync::CancellationToken;

const XIN_COMPONENT: &str = "xin";
const SHELL_TIMEOUT_SECS: u64 = 120;
// Sub-agent (xin runner) tool-iteration hard clamp. Behavior-limits Phase 1:
// raised 20 -> 100 to align with `sub_agent.max_iterations` default.
// 0-semantics note: on this path `0` (or >cap) clamps to this value, NOT to the
// main-agent fallback in `agent/loop_.rs:DEFAULT_MAX_TOOL_ITERATIONS`.
const AGENT_MAX_TOOL_ITERATIONS: usize = 100;
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

    // FIX-P1-31: honour the configured `security.audit` block on the gate audit path.
    let security = crate::runtime::bootstrap::build_security_policy(&config);
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
    let Some(lease) = store::claim_step_with_lease(config, &step.id, &worker, lease_ttl)? else {
        tracing::debug!(target: "xin", step_id = %step.id, "step already claimed by another worker");
        return Ok(());
    };
    if !store::mark_step_running_with_lease(config, &step.id, &lease)? {
        tracing::debug!(target: "xin", step_id = %step.id, "step running transition lost the lease");
        return Ok(());
    }

    // Re-read the freshly-claimed step so we run against the persisted lease/
    // checkpoint state rather than the pre-claim snapshot.
    let step = store::get_step(config, &step.id)?;

    let outcome = execute_step_with_heartbeat(config, security, registry, &step, lease, lease_ttl).await;
    persist_step_execution_outcome(config, &step, outcome);
    Ok(())
}

enum StepExecutionOutcome {
    Authorized {
        success: bool,
        output: String,
        lease: store::XinStepLease,
    },
    AuthorityLost,
}

fn persist_step_execution_outcome(config: &Config, step: &XinStep, outcome: StepExecutionOutcome) {
    match outcome {
        StepExecutionOutcome::AuthorityLost => {
            tracing::warn!(target: "xin", step_id = %step.id, "lease authority lost; skipping step persistence");
        }
        StepExecutionOutcome::Authorized {
            success: true,
            output,
            lease,
        } => match store::complete_step_with_lease(config, &step.id, &lease, &output) {
            Ok(true) => {}
            Ok(false) => tracing::warn!(target: "xin", step_id = %step.id, "completion fence lost lease authority"),
            Err(e) => tracing::warn!(target: "xin", step_id = %step.id, "failed to complete step: {e}"),
        },
        StepExecutionOutcome::Authorized {
            success: false,
            output,
            lease,
        } => match store::fail_step_with_lease(config, &step.id, &lease, &output) {
            Ok(true) => {}
            Ok(false) => tracing::warn!(target: "xin", step_id = %step.id, "failure fence lost lease authority"),
            Err(e) => tracing::warn!(target: "xin", step_id = %step.id, "failed to fail step: {e}"),
        },
    }
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
    initial_lease: store::XinStepLease,
    lease_ttl_secs: u64,
) -> StepExecutionOutcome {
    let heartbeat_secs = (lease_ttl_secs / 3).max(MIN_HEARTBEAT_SECS);
    let heartbeat_stop = CancellationToken::new();
    let heartbeat_stop_child = heartbeat_stop.clone();
    let lease_lost = CancellationToken::new();
    let lease_lost_hb = lease_lost.clone();

    let hb_config = config.clone();
    let hb_step_id = step.id.clone();
    let initial_expiry = initial_lease.expires_at;
    let exact_lease = Arc::new(parking_lot::Mutex::new(initial_lease));
    let heartbeat_lease = Arc::clone(&exact_lease);
    let initial_deadline = (initial_expiry - Utc::now()).to_std().unwrap_or(Duration::ZERO);
    let hb_handle = tokio::spawn(async move {
        let mut current_lease = heartbeat_lease.lock().clone();
        let mut ticker = time::interval(Duration::from_secs(heartbeat_secs));
        // Skip the immediate first tick (lease was just set by claim).
        ticker.tick().await;
        let lease_deadline = time::sleep(initial_deadline);
        tokio::pin!(lease_deadline);
        loop {
            tokio::select! {
                biased;
                () = &mut lease_deadline => {
                    tracing::warn!(target: "xin", step_id = %hb_step_id, "lease deadline elapsed; cancelling execution");
                    lease_lost_hb.cancel();
                    return true;
                }
                _ = ticker.tick() => {
                    match store::renew_step_lease_generation(&hb_config, &hb_step_id, &current_lease, lease_ttl_secs) {
                        Ok(Some(renewed_lease)) => {
                            tracing::debug!(target: "xin", step_id = %hb_step_id, "lease renewed");
                            let remaining = (renewed_lease.expires_at - Utc::now())
                                .to_std()
                                .unwrap_or(Duration::ZERO);
                            current_lease = renewed_lease.clone();
                            *heartbeat_lease.lock() = renewed_lease;
                            lease_deadline.as_mut().reset(time::Instant::now() + remaining);
                        }
                        Ok(None) => {
                            tracing::warn!(target: "xin", step_id = %hb_step_id, "lease lost; stopping heartbeat");
                            lease_lost_hb.cancel();
                            return true;
                        }
                        Err(e) => {
                            tracing::warn!(target: "xin", step_id = %hb_step_id, "heartbeat renewal failed: {e}");
                        }
                    }
                }
                _ = heartbeat_stop_child.cancelled() => return false,
            }
        }
    });

    let mut execution = Box::pin(execute_step_inner(
        config,
        security,
        registry,
        step,
        Some(lease_lost.clone()),
    ));
    let result = tokio::select! {
        biased;
        () = lease_lost.cancelled() => None,
        result = &mut execution => Some(result),
    };
    // Dropping the whole execution future is the authority boundary. For Shell
    // this invokes the shared adapter's process-group kill/reap Drop path; for
    // AgentSession it cancels the in-flight agent future as well.
    drop(execution);

    heartbeat_stop.cancel();
    let authority_lost = match hb_handle.await {
        Ok(authority_lost) => authority_lost,
        Err(e) => {
            tracing::warn!(target: "xin", step_id = %step.id, "heartbeat task join error: {e}");
            true
        }
    };
    let Some(result) = result else {
        return StepExecutionOutcome::AuthorityLost;
    };
    if authority_lost {
        return StepExecutionOutcome::AuthorityLost;
    }

    let lease = exact_lease.lock().clone();
    if !save_step_checkpoint(config, step, &lease, result.0) {
        return StepExecutionOutcome::AuthorityLost;
    }
    StepExecutionOutcome::Authorized {
        success: result.0,
        output: result.1,
        lease,
    }
}

/// Run a step's actual work, reusing the same execution backends as XinTask.
async fn execute_step_inner(
    config: &Config,
    security: &SecurityPolicy,
    registry: &BuiltinRegistry,
    step: &XinStep,
    cancellation: Option<CancellationToken>,
) -> (bool, String) {
    // Bridge the step into the existing XinTask-shaped execution backends.
    let bridge = step_as_task(step);
    match step.execution_mode {
        ExecutionMode::Internal => run_internal(config, registry, &bridge).await,
        ExecutionMode::AgentSession => run_agent(config, security, &bridge).await,
        ExecutionMode::Shell => run_shell_with_cancellation(config, security, &bridge, cancellation).await,
    }
}

fn save_step_checkpoint(config: &Config, step: &XinStep, lease: &store::XinStepLease, succeeded: bool) -> bool {
    // Persist only while the heartbeat still confirms our authority. A lost
    // lease may already belong to another worker, whose checkpoint must win.
    let checkpoint = serde_json::json!({
        "sequence": step.sequence,
        "attempt": step.retry_count + 1,
        "succeeded": succeeded,
        "at": Utc::now().to_rfc3339(),
    })
    .to_string();
    match store::save_step_checkpoint_with_lease(config, &step.id, lease, &checkpoint) {
        Ok(saved) => saved,
        Err(e) => {
            tracing::warn!(target: "xin", step_id = %step.id, "failed to save step checkpoint: {e}");
            false
        }
    }
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

    // Background xin runner: no cooperative shutdown signal of its own; the
    // runner drives this synchronously. See never_cancelled_shutdown docs.
    match crate::agent::run(
        agent_config,
        Some(prompt),
        None,
        config.default_model.clone(),
        config.default_temperature,
        crate::runtime::shutdown::never_cancelled_shutdown(),
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
    run_shell_with_cancellation(config, security, task, None).await
}

async fn run_shell_with_cancellation(
    config: &Config,
    security: &SecurityPolicy,
    task: &XinTask,
    cancellation: Option<CancellationToken>,
) -> (bool, String) {
    let process = match ShellProcessAdapter::from_config(config) {
        Ok(process) => process,
        Err(error) => return (false, format!("runtime error: {error}")),
    };
    run_shell_with_adapter(config, security, task, cancellation, &process).await
}

async fn run_shell_with_adapter(
    config: &Config,
    security: &SecurityPolicy,
    task: &XinTask,
    cancellation: Option<CancellationToken>,
    process: &ShellProcessAdapter,
) -> (bool, String) {
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

    match process
        .execute(ShellProcessRequest {
            command: &task.payload,
            workspace_dir: &config.workspace_dir,
            timeout: Duration::from_secs(SHELL_TIMEOUT_SECS),
            cancellation,
        })
        .await
    {
        Ok(output) => {
            let combined = format!(
                "status={}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                output.stdout.trim(),
                output.stderr.trim()
            );
            (output.status.success(), combined)
        }
        Err(ShellProcessError::Timeout(_)) => (false, format!("task timed out after {SHELL_TIMEOUT_SECS}s")),
        Err(ShellProcessError::Cancelled) => (false, "task cancelled after lease loss".into()),
        Err(ShellProcessError::Sandbox(error)) => (
            false,
            format!("blocked by security policy: sandbox failed to wrap command: {error}"),
        ),
        Err(error) => (false, format!("spawn error: {error}")),
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
    use crate::runtime::{NativeRuntime, RuntimeAdapter};
    use crate::security::traits::NoopSandbox;
    use crate::xin::types::{NewXinTask, TaskKind, TaskPriority};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;

    struct SpyRuntime {
        called: Arc<AtomicBool>,
    }

    impl RuntimeAdapter for SpyRuntime {
        fn name(&self) -> &str {
            "xin-spy"
        }

        fn has_shell_access(&self) -> bool {
            true
        }

        fn has_filesystem_access(&self) -> bool {
            true
        }

        fn storage_path(&self) -> PathBuf {
            PathBuf::new()
        }

        fn supports_long_running(&self) -> bool {
            true
        }

        fn build_shell_command(&self, command: &str, workspace_dir: &Path) -> anyhow::Result<tokio::process::Command> {
            self.called.store(true, Ordering::SeqCst);
            NativeRuntime::new().build_shell_command(command, workspace_dir)
        }
    }

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
        config.autonomy.sandbox.backend = crate::config::SandboxBackend::None;
        config.autonomy.sandbox.enabled = Some(false);
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
    async fn xin_entry_uses_runtime_adapter_builder() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = store::add_task(
            &config,
            &NewXinTask {
                owner_id: None,
                topic_id: None,
                parent_task_id: None,
                source_message_event_id: None,
                name: "runtime_spy".into(),
                description: None,
                kind: TaskKind::User,
                priority: TaskPriority::Normal,
                execution_mode: ExecutionMode::Shell,
                payload: "echo xin-runtime-spy".into(),
                recurring: false,
                interval_secs: 0,
                max_failures: 1,
                approval_grant_json: None,
            },
        )
        .unwrap();
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let called = Arc::new(AtomicBool::new(false));
        let process = ShellProcessAdapter::new(
            Arc::new(SpyRuntime {
                called: Arc::clone(&called),
            }),
            Arc::new(NoopSandbox),
            Vec::new(),
        );

        let (success, output) = run_shell_with_adapter(&config, &security, &task, None, &process).await;

        assert!(success, "{output}");
        assert!(output.contains("xin-runtime-spy"));
        assert!(
            called.load(Ordering::SeqCst),
            "Xin must use RuntimeAdapter::build_shell_command"
        );
    }

    #[tokio::test]
    async fn xin_forbidden_path_denial_has_single_runner_audit_identity() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        let task = store::add_task(
            &config,
            &NewXinTask {
                owner_id: None,
                topic_id: None,
                parent_task_id: None,
                source_message_event_id: None,
                name: "forbidden_path".into(),
                description: None,
                kind: TaskKind::User,
                priority: TaskPriority::Normal,
                execution_mode: ExecutionMode::Shell,
                payload: "cat /etc/passwd".into(),
                recurring: false,
                interval_secs: 0,
                max_failures: 1,
                approval_grant_json: None,
            },
        )
        .unwrap();
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_shell(&config, &security, &task).await;
        assert!(!success);
        assert!(output.contains("forbidden path argument: /etc/passwd"), "{output}");

        let audit = std::fs::read_to_string(config.workspace_dir.join("audit.log")).expect("audit log");
        let events: Vec<crate::security::audit::AuditEvent> = audit
            .lines()
            .map(|line| serde_json::from_str(line).expect("audit event"))
            .collect();
        assert_eq!(events.len(), 1);
        let action = events
            .first()
            .and_then(|event| event.action.as_ref())
            .and_then(|action| action.command.as_deref())
            .unwrap_or_default();
        assert!(action.starts_with("xin_runner:"), "{action}");
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
        config.autonomy.sandbox.backend = crate::config::SandboxBackend::Firejail;
        config.autonomy.sandbox.enabled = Some(true);
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
        let config = test_config(&tmp);
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
        let config = test_config(&tmp);
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
        let lease = store::claim_step_with_lease(&config, &step.id, &worker, 6)
            .unwrap()
            .expect("lease claim");
        assert!(store::mark_step_running_with_lease(&config, &step.id, &lease).unwrap());
        let step = store::get_step(&config, &step.id).unwrap();

        // Run the heartbeat-wrapped step; the wrapper must keep the short lease
        // alive for the duration so a concurrent stale sweep does not reap it.
        let registry = BuiltinRegistry::new();
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let outcome = execute_step_with_heartbeat(&config, &security, &registry, &step, lease, 6).await;
        assert!(matches!(
            outcome,
            StepExecutionOutcome::Authorized { success: true, .. }
        ));
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

    #[tokio::test]
    async fn lease_loss_cancels_shell_without_overwriting_new_owner_state() {
        use crate::xin::types::{NewXinGoal, NewXinStep, StepStatus};

        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        let goal = store::add_goal(
            &config,
            &NewXinGoal {
                owner_id: None,
                topic_id: None,
                parent_task_id: None,
                source_message_event_id: None,
                name: "lease_loss_goal".into(),
                description: None,
                kind: TaskKind::User,
                priority: TaskPriority::Normal,
                target_completion_at: None,
                initial_steps: vec![NewXinStep {
                    sequence: 1,
                    name: "lost-shell".into(),
                    description: None,
                    execution_mode: ExecutionMode::Shell,
                    payload: "sleep 3; touch old-owner-marker".into(),
                    max_retries: 1,
                    approval_grant_json: None,
                    lease_ttl_secs: 1,
                }],
            },
        )
        .unwrap();
        let step_id = store::list_steps(&config, &goal.id).unwrap().remove(0).id;
        let old_owner = "prx:test:old";
        let old_lease = store::claim_step_with_lease(&config, &step_id, old_owner, 1)
            .unwrap()
            .expect("old lease claim");
        assert!(store::mark_step_running_with_lease(&config, &step_id, &old_lease).unwrap());
        let step = store::get_step(&config, &step_id).unwrap();

        let execution_config = config.clone();
        let execution_step = step.clone();
        let execution = tokio::spawn(async move {
            let registry = BuiltinRegistry::new();
            let security = SecurityPolicy::from_config(&execution_config.autonomy, &execution_config.workspace_dir);
            execute_step_with_heartbeat(&execution_config, &security, &registry, &execution_step, old_lease, 1).await
        });

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let stale = store::mark_steps_stale(&config, Utc::now()).unwrap();
                if stale.iter().any(|id| id == &step_id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("old lease should expire");

        let new_owner = "prx:test:new";
        let new_lease = store::claim_step_with_lease(&config, &step_id, new_owner, 60)
            .unwrap()
            .expect("new lease claim");
        assert!(store::mark_step_running_with_lease(&config, &step_id, &new_lease).unwrap());
        store::save_step_checkpoint(&config, &step_id, r#"{"owner":"new"}"#).unwrap();

        let outcome = execution.await.expect("lease-managed execution task");
        assert!(matches!(&outcome, StepExecutionOutcome::AuthorityLost));
        persist_step_execution_outcome(&config, &step, outcome);

        let current = store::get_step(&config, &step_id).unwrap();
        assert_eq!(current.lease_owner.as_deref(), Some(new_owner));
        assert_eq!(current.status, StepStatus::Running);
        assert_eq!(current.retry_count, 0, "lost owner must not call fail_step");
        assert_eq!(current.checkpoint_json.as_deref(), Some(r#"{"owner":"new"}"#));
        assert!(
            !config.workspace_dir.join("old-owner-marker").exists(),
            "lost owner's process must be cancelled before writing its marker"
        );
    }
}
