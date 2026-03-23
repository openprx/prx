//! Tick-based heartbeat runner for the xin (心) autonomous task engine.
//!
//! Follows the cron/scheduler.rs pattern:
//! - Periodic interval tick
//! - Query due tasks from SQLite
//! - Execute concurrently (buffer_unordered)
//! - Persist results and reschedule

use crate::config::Config;
use crate::security::SecurityPolicy;
use crate::xin::builtin::BuiltinRegistry;
use crate::xin::store;
use crate::xin::types::{ExecutionMode, XinTask, XinTickSummary};
use anyhow::Result;
use chrono::Utc;
use futures_util::{StreamExt, stream};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{self, Duration};

const XIN_COMPONENT: &str = "xin";
const SHELL_TIMEOUT_SECS: u64 = 120;
const AGENT_MAX_TOOL_ITERATIONS: usize = 20;

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

    if !security.is_command_allowed(&task.payload) {
        return (
            false,
            format!("blocked by security policy: command not allowed: {}", task.payload),
        );
    }

    if !security.record_action() {
        return (false, "blocked by security policy: action budget exhausted".into());
    }

    let child = match Command::new("sh")
        .arg("-lc")
        .arg(&task.payload)
        .current_dir(&config.workspace_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xin::types::{NewXinTask, TaskKind, TaskPriority};
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
            name: "shell_test".into(),
            description: None,
            kind: TaskKind::User,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Shell,
            payload: "echo hello".into(),
            recurring: false,
            interval_secs: 0,
            max_failures: 3,
        };
        let task = store::add_task(&config, &new).unwrap();

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (success, output) = run_shell(&config, &security, &task).await;

        assert!(success, "shell task should succeed: {output}");
        assert!(output.contains("hello"));
    }

    #[tokio::test]
    async fn execute_shell_task_failure() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let new = NewXinTask {
            name: "shell_fail".into(),
            description: None,
            kind: TaskKind::User,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Shell,
            payload: "exit 1".into(),
            recurring: false,
            interval_secs: 0,
            max_failures: 3,
        };
        let task = store::add_task(&config, &new).unwrap();

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (success, _output) = run_shell(&config, &security, &task).await;

        assert!(!success);
    }

    #[tokio::test]
    async fn execute_internal_health_check() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let registry = BuiltinRegistry::new();

        let new = NewXinTask {
            name: "xin:health_check".into(),
            description: None,
            kind: TaskKind::System,
            priority: TaskPriority::High,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:health_check".into(),
            recurring: true,
            interval_secs: 300,
            max_failures: 10,
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
            name: "xin:stale_cleanup".into(),
            description: None,
            kind: TaskKind::System,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:stale_cleanup".into(),
            recurring: true,
            interval_secs: 1800,
            max_failures: 10,
        };
        let task = store::add_task(&config, &new).unwrap();

        let (success, output) = run_internal(&config, &registry, &task).await;
        assert!(success, "stale cleanup should succeed: {output}");
        assert!(output.contains("stale cleanup"));
    }
}
