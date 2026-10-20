#![cfg(test)]

use super::*;
use tokio::test;

#[test]
async fn config_rejects_empty_agentic_delegate_allowlist() {
    let mut config = Config::default();
    config
        .agents
        .insert("worker".into(), agentic_delegate_config(Vec::new()));

    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("agents.worker.allowed_tools must not be empty"));
}

#[test]
async fn self_system_evolution_interval_hours_deserializes() {
    let raw = r#"
default_temperature = 0.7

[self_system]
evolution_interval_hours = 12
"#;
    let parsed: Config = toml::from_str(raw).unwrap();
    assert_eq!(parsed.self_system.evolution_interval_hours, 12);
}

#[test]
async fn self_system_fitness_calendar_config_deserializes() {
    let raw = r#"
default_temperature = 0.7

[self_system]
fitness_interval_hours = 8760

[self_system.fitness]
auto_run = false
timezone = "Asia/Tbilisi"
run_at = "00:20"
retention_days = 90
max_backfill_days = 3
min_samples = 8
min_coverage = 0.75
"#;
    let parsed: Config = toml::from_str(raw).unwrap();
    assert_eq!(parsed.self_system.fitness_interval_hours, 8760);
    assert!(!parsed.self_system.fitness.auto_run);
    assert_eq!(parsed.self_system.fitness.timezone, "Asia/Tbilisi");
    assert_eq!(parsed.self_system.fitness.run_at, "00:20");
    assert_eq!(parsed.self_system.fitness.retention_days, 90);
    assert_eq!(parsed.self_system.fitness.max_backfill_days, 3);
    assert_eq!(parsed.self_system.fitness.min_samples, 8);
    assert!((parsed.self_system.fitness.min_coverage - 0.75).abs() < f64::EPSILON);
}

#[test]
async fn agent_config_defaults() {
    let cfg = AgentConfig::default();
    assert!(!cfg.compact_context);
    assert_eq!(cfg.read_only_tool_concurrency_window, 2);
    assert!(!cfg.priority_scheduling_enabled);
    assert_eq!(cfg.low_priority_tools, default_agent_low_priority_tools());
}

#[test]
async fn empty_agent_compaction_section_keeps_os_paging_enabled() {
    // Phase 1: an empty `[agent.compaction]` table (no `os_paging` key) must
    // still deserialize with os_paging enabled, because the field's
    // `#[serde(default)]` resolves to `OsPagingConfig::default()` (enabled = true).
    let raw = r#"
default_temperature = 0.7
[agent.compaction]
"#;
    let parsed: Config = toml::from_str(raw).unwrap();
    assert_eq!(parsed.agent.compaction.os_paging.max_recalled_pages, 10);
}

#[test]
async fn agent_config_deserializes() {
    let raw = r#"
default_temperature = 0.7
[agent]
compact_context = true
read_only_tool_concurrency_window = 4
priority_scheduling_enabled = true
low_priority_tools = ["sessions_spawn", "delegate"]
"#;
    let parsed: Config = toml::from_str(raw).unwrap();
    assert!(parsed.agent.compact_context);
    assert_eq!(parsed.agent.read_only_tool_concurrency_window, 4);
    assert!(parsed.agent.priority_scheduling_enabled);
    assert_eq!(parsed.agent.low_priority_tools, vec!["sessions_spawn", "delegate"]);
}
