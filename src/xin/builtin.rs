//! Built-in system task definitions and handler registry for xin (心).
//!
//! Each built-in task maps to an async handler that invokes existing PRX
//! infrastructure (health checks, stale cleanup, evolution, fitness).

use crate::config::Config;
use crate::xin::types::{ExecutionMode, NewXinTask, TaskKind, TaskPriority};
use anyhow::Result;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

/// Async handler signature: `(config) -> Result<String>`.
type HandlerFn = fn(Config) -> Pin<Box<dyn Future<Output = Result<String>> + Send>>;

/// Registry of built-in system task handlers.
pub struct BuiltinRegistry {
    handlers: HashMap<&'static str, HandlerFn>,
}

impl BuiltinRegistry {
    pub fn new() -> Self {
        let mut handlers: HashMap<&'static str, HandlerFn> = HashMap::new();
        handlers.insert("xin:health_check", |cfg| Box::pin(handle_health_check(cfg)));
        handlers.insert("xin:stale_cleanup", |cfg| Box::pin(handle_stale_cleanup(cfg)));
        handlers.insert("xin:memory_evolution", |cfg| Box::pin(handle_memory_evolution(cfg)));
        handlers.insert("xin:fitness_report", |cfg| Box::pin(handle_fitness_report(cfg)));
        handlers.insert("xin:memory_hygiene", |cfg| Box::pin(handle_memory_hygiene(cfg)));
        Self { handlers }
    }

    /// Execute a built-in handler by name.
    pub async fn execute(&self, name: &str, config: Config) -> Result<String> {
        match self.handlers.get(name) {
            Some(handler) => handler(config).await,
            None => anyhow::bail!("Unknown built-in handler: {name}"),
        }
    }

    /// Check if a handler exists for the given name.
    #[cfg(test)]
    pub fn has_handler(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }
}

/// Definitions for built-in system tasks to be registered via `ensure_system_task`.
pub fn builtin_task_definitions() -> Vec<NewXinTask> {
    vec![
        NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "xin:health_check".into(),
            description: Some("Check all component health statuses".into()),
            kind: TaskKind::System,
            priority: TaskPriority::High,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:health_check".into(),
            recurring: true,
            interval_secs: 300, // 5 minutes
            max_failures: 10,
            approval_grant_json: None,
        },
        NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "xin:stale_cleanup".into(),
            description: Some("Clean up stale and completed non-recurring tasks".into()),
            kind: TaskKind::System,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:stale_cleanup".into(),
            recurring: true,
            interval_secs: 1800, // 30 minutes
            max_failures: 10,
            approval_grant_json: None,
        },
        NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "xin:memory_evolution".into(),
            description: Some("Trigger L1/L2/L3 memory evolution cycles".into()),
            kind: TaskKind::System,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:memory_evolution".into(),
            recurring: true,
            interval_secs: 10800, // 3 hours
            max_failures: 5,
            approval_grant_json: None,
        },
        NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "xin:fitness_report".into(),
            description: Some("Generate daily fitness/adaptation report".into()),
            kind: TaskKind::System,
            priority: TaskPriority::Low,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:fitness_report".into(),
            recurring: true,
            interval_secs: 86400, // 24 hours
            max_failures: 5,
            approval_grant_json: None,
        },
        NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "xin:memory_hygiene".into(),
            description: Some("Memory compaction, deduplication, pruning".into()),
            kind: TaskKind::System,
            priority: TaskPriority::Low,
            execution_mode: ExecutionMode::Internal,
            payload: "xin:memory_hygiene".into(),
            recurring: true,
            interval_secs: 43200, // 12 hours
            max_failures: 5,
            approval_grant_json: None,
        },
    ]
}

// ── Handlers ────────────────────────────────────────────────────────────

#[allow(clippy::unused_async)]
async fn handle_health_check(_config: Config) -> Result<String> {
    let snapshot = crate::health::snapshot_json();
    let summary =
        serde_json::to_string_pretty(&snapshot).unwrap_or_else(|_| "failed to serialize health snapshot".into());
    Ok(format!("health check completed\n{summary}"))
}

#[allow(clippy::unused_async)]
async fn handle_stale_cleanup(config: Config) -> Result<String> {
    let stale_count = crate::xin::store::mark_stale(&config, config.xin.stale_timeout_minutes)?;
    let removed = crate::xin::store::remove_completed(&config)?;
    Ok(format!(
        "stale cleanup: marked {stale_count} stale, removed {removed} completed"
    ))
}

#[allow(clippy::unused_async)]
async fn handle_memory_evolution(config: Config) -> Result<String> {
    if !config.self_system.evolution_enabled {
        return Ok("memory evolution skipped: evolution not enabled".into());
    }

    let scheduler = crate::xin::evolution::DraftEvolutionScheduler::load(config)?;
    let report = scheduler.tick()?;
    Ok(format!(
        "evolution draft tick completed: mode={:?}, drafted={}, judged={}, applied={}",
        report.mode, report.drafted, report.judged, report.applied
    ))
}

async fn handle_fitness_report(config: Config) -> Result<String> {
    let report = crate::self_system::fitness::run_fitness_report_with_config(&config).await?;
    Ok(format!(
        "fitness report: score={:.3}, confidence={:.3}, date={}",
        report.final_score, report.confidence, report.window.date
    ))
}

#[allow(clippy::unused_async)]
async fn handle_memory_hygiene(config: Config) -> Result<String> {
    if !config.self_system.enabled {
        return Ok("memory hygiene skipped: self_system not enabled".into());
    }

    crate::memory::hygiene::run_if_due(&config.memory, &config.workspace_dir)?;
    Ok("memory hygiene completed: deterministic hygiene tick".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_all_builtin_handlers() {
        let registry = BuiltinRegistry::new();
        for def in builtin_task_definitions() {
            assert!(
                registry.has_handler(&def.payload),
                "Missing handler for builtin task: {}",
                def.payload
            );
        }
    }

    #[test]
    fn builtin_definitions_are_all_system_kind() {
        for def in builtin_task_definitions() {
            assert_eq!(def.kind, TaskKind::System);
            assert!(def.recurring);
            assert!(def.interval_secs > 0);
            assert_eq!(def.execution_mode, ExecutionMode::Internal);
        }
    }

    #[test]
    fn registry_rejects_unknown_handler() {
        let registry = BuiltinRegistry::new();
        assert!(!registry.has_handler("xin:nonexistent"));
    }
}
