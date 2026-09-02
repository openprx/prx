//! Operator-facing CLI for Xin task and Goal/Step management.

#![allow(clippy::print_stdout)]

use super::{runner, store};
use crate::config::Config;
use crate::security::policy::{ApprovalGrant, PERSISTED_APPROVAL_GRANT_TTL_SECS};
use crate::xin::types::{
    ExecutionMode, GoalStatus, NewXinGoal, NewXinStep, NewXinTask, StepStatus, TaskKind, TaskPriority, TaskStatus,
    XinTaskPatch,
};
use crate::{XinCommands, XinGoalCommands, XinStepCommands};
use anyhow::{Result, bail};

fn execution_mode(value: &str) -> ExecutionMode {
    match value {
        "shell" => ExecutionMode::Shell,
        _ => ExecutionMode::AgentSession,
    }
}

fn cli_shell_grant(mode: &ExecutionMode, payload: &str) -> Result<Option<String>> {
    if *mode != ExecutionMode::Shell {
        return Ok(None);
    }
    let grant = ApprovalGrant::persisted_for_command(
        "xin_runner",
        payload,
        "xin_cli",
        Some("local-operator".to_string()),
        PERSISTED_APPROVAL_GRANT_TTL_SECS,
    );
    Ok(Some(serde_json::to_string(&grant)?))
}

pub async fn handle_command(command: XinCommands, config: &Config) -> Result<()> {
    match command {
        XinCommands::Status => {
            let tasks = store::list_tasks(config)?;
            let goals = store::list_goals(config)?;
            let mut steps = Vec::new();
            for goal in &goals {
                steps.extend(store::list_steps(config, &goal.id)?);
            }
            println!("Xin status");
            println!(
                "  Tasks: {} total, {} active, {} paused, {} cancelled",
                tasks.len(),
                tasks.iter().filter(|task| task.enabled).count(),
                tasks
                    .iter()
                    .filter(|task| !task.enabled && task.status != TaskStatus::Cancelled)
                    .count(),
                tasks.iter().filter(|task| task.status == TaskStatus::Cancelled).count(),
            );
            println!(
                "  Goals: {} total, {} active, {} completed, {} cancelled",
                goals.len(),
                goals
                    .iter()
                    .filter(|goal| goal.enabled && matches!(goal.status, GoalStatus::Pending | GoalStatus::Running))
                    .count(),
                goals.iter().filter(|goal| goal.status == GoalStatus::Completed).count(),
                goals.iter().filter(|goal| goal.status == GoalStatus::Cancelled).count(),
            );
            println!(
                "  Steps: {} total, {} waiting, {} running, {} completed",
                steps.len(),
                steps
                    .iter()
                    .filter(|step| matches!(step.status, StepStatus::Pending | StepStatus::Stale))
                    .count(),
                steps
                    .iter()
                    .filter(|step| matches!(step.status, StepStatus::Claimed | StepStatus::Running))
                    .count(),
                steps.iter().filter(|step| step.status == StepStatus::Completed).count(),
            );
            Ok(())
        }
        XinCommands::List => {
            let tasks = store::list_tasks(config)?;
            if tasks.is_empty() {
                println!("No xin tasks.");
            }
            for task in tasks {
                println!(
                    "{} | {} | {} | enabled={} | mode={} | next={}",
                    task.id,
                    task.name,
                    task.status.as_str(),
                    task.enabled,
                    task.execution_mode.as_str(),
                    task.next_run_at.to_rfc3339(),
                );
            }
            Ok(())
        }
        XinCommands::Get { id } => {
            println!("{}", serde_json::to_string_pretty(&store::get_task(config, &id)?)?);
            Ok(())
        }
        XinCommands::Add {
            name,
            payload,
            description,
            execution_mode: mode,
            priority,
            recurring,
            interval_secs,
        } => {
            if recurring && interval_secs < 60 {
                bail!("Recurring tasks require --interval-secs >= 60");
            }
            let mode = execution_mode(&mode);
            let task = store::add_task(
                config,
                &NewXinTask {
                    owner_id: None,
                    topic_id: None,
                    parent_task_id: None,
                    source_message_event_id: None,
                    name,
                    description,
                    kind: TaskKind::User,
                    priority: TaskPriority::from_str_lossy(&priority),
                    execution_mode: mode.clone(),
                    approval_grant_json: cli_shell_grant(&mode, &payload)?,
                    payload,
                    recurring,
                    interval_secs,
                    max_failures: 0,
                },
            )?;
            println!("Created xin task {} ({})", task.id, task.name);
            Ok(())
        }
        XinCommands::Update {
            id,
            name,
            description,
            priority,
            payload,
            interval_secs,
        } => {
            if name.is_none()
                && description.is_none()
                && priority.is_none()
                && payload.is_none()
                && interval_secs.is_none()
            {
                bail!("At least one update option is required");
            }
            let existing = store::get_task(config, &id)?;
            if existing.recurring && interval_secs.is_some_and(|interval| interval < 60) {
                bail!("Recurring tasks require --interval-secs >= 60");
            }
            let approval_grant_json = payload
                .as_deref()
                .map(|command| cli_shell_grant(&existing.execution_mode, command))
                .transpose()?
                .flatten();
            let task = store::update_task(
                config,
                &id,
                &XinTaskPatch {
                    name,
                    description,
                    priority: priority.as_deref().map(TaskPriority::from_str_lossy),
                    payload,
                    interval_secs,
                    approval_grant_json,
                    ..XinTaskPatch::default()
                },
            )?;
            println!("Updated xin task {} ({})", task.id, task.name);
            Ok(())
        }
        XinCommands::Remove { id } => {
            store::remove_task(config, &id)?;
            println!("Removed xin task {id}");
            Ok(())
        }
        XinCommands::Pause { id } => {
            store::pause_task(config, &id)?;
            println!("Paused xin task {id}");
            Ok(())
        }
        XinCommands::Resume { id } => {
            store::resume_task(config, &id)?;
            println!("Resumed xin task {id}");
            Ok(())
        }
        XinCommands::Cancel { id } => {
            store::cancel_task(config, &id)?;
            println!("Cancelled xin task {id}");
            Ok(())
        }
        XinCommands::Run { id } => {
            let security = crate::runtime::bootstrap::build_security_policy(config);
            let run = runner::run_task_now(config, &security, &id).await?;
            println!("{}", serde_json::to_string_pretty(&run_output(&run))?);
            if !run.success {
                bail!("Xin task execution failed");
            }
            Ok(())
        }
        XinCommands::Runs { id, limit } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&store::list_runs(config, &id, limit as usize)?)?
            );
            Ok(())
        }
        XinCommands::Events { id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&store::list_task_events(config, &id)?)?
            );
            Ok(())
        }
        XinCommands::Goals { goal_command } => handle_goal_command(goal_command, config),
        XinCommands::Steps { step_command } => handle_step_command(step_command, config),
    }
}

fn run_output(run: &runner::XinManualRun) -> serde_json::Value {
    serde_json::json!({
        "task_id": run.task_id,
        "success": run.success,
        "output": run.output,
    })
}

fn handle_goal_command(command: XinGoalCommands, config: &Config) -> Result<()> {
    match command {
        XinGoalCommands::List => {
            println!("{}", serde_json::to_string_pretty(&store::list_goals(config)?)?);
        }
        XinGoalCommands::Get { id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "goal": store::get_goal(config, &id)?,
                    "steps": store::list_steps(config, &id)?,
                }))?
            );
        }
        XinGoalCommands::Add {
            name,
            description,
            priority,
            target_completion_at,
        } => {
            let target_completion_at = target_completion_at
                .map(|value| {
                    chrono::DateTime::parse_from_rfc3339(&value)
                        .map(|time| time.with_timezone(&chrono::Utc))
                        .map_err(anyhow::Error::new)
                })
                .transpose()?;
            let goal = store::add_goal(
                config,
                &NewXinGoal {
                    owner_id: None,
                    topic_id: None,
                    parent_task_id: None,
                    source_message_event_id: None,
                    name,
                    description,
                    kind: TaskKind::User,
                    priority: TaskPriority::from_str_lossy(&priority),
                    target_completion_at,
                    initial_steps: Vec::new(),
                },
            )?;
            println!("Created xin goal {} ({})", goal.id, goal.name);
        }
        XinGoalCommands::Pause { id } => {
            store::pause_goal(config, &id)?;
            println!("Paused xin goal {id}");
        }
        XinGoalCommands::Resume { id } => {
            store::resume_goal(config, &id)?;
            println!("Resumed xin goal {id}");
        }
        XinGoalCommands::Cancel { id } => {
            store::cancel_goal(config, &id)?;
            println!("Cancelled xin goal {id}");
        }
        XinGoalCommands::Remove { id } => {
            store::remove_goal(config, &id)?;
            println!("Removed xin goal {id}");
        }
    }
    Ok(())
}

fn handle_step_command(command: XinStepCommands, config: &Config) -> Result<()> {
    match command {
        XinStepCommands::List { goal_id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&store::list_steps(config, &goal_id)?)?
            );
        }
        XinStepCommands::Get { id } => {
            println!("{}", serde_json::to_string_pretty(&store::get_step(config, &id)?)?);
        }
        XinStepCommands::Add {
            goal_id,
            name,
            payload,
            description,
            sequence,
            execution_mode: mode,
            lease_ttl_secs,
        } => {
            let sequence = sequence.unwrap_or(
                store::list_steps(config, &goal_id)?
                    .iter()
                    .map(|step| step.sequence)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1),
            );
            let mode = execution_mode(&mode);
            let step = store::add_step(
                config,
                &goal_id,
                &NewXinStep {
                    sequence,
                    name,
                    description,
                    execution_mode: mode.clone(),
                    approval_grant_json: cli_shell_grant(&mode, &payload)?,
                    payload,
                    max_retries: 0,
                    lease_ttl_secs,
                },
            )?;
            println!(
                "Created xin step {} (goal {}, sequence {})",
                step.id, step.goal_id, step.sequence
            );
        }
        XinStepCommands::Retry { id } => {
            let step = store::retry_step(config, &id)?;
            println!("Retry queued for xin step {}", step.id);
        }
    }
    Ok(())
}
