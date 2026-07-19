use crate::config::Config;
use crate::config::HeartbeatConfig;
use crate::xin::types::{ExecutionMode, NewXinTask, TaskKind, TaskPriority, XinTask, XinTaskPatch};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

pub const HEARTBEAT_TASK_PREFIX: &str = "heartbeat:";

/// HEARTBEAT.md parser and Xin materialization adapter.
pub struct HeartbeatEngine {
    config: HeartbeatConfig,
    workspace_dir: std::path::PathBuf,
}

impl HeartbeatEngine {
    pub const fn new(config: HeartbeatConfig, workspace_dir: std::path::PathBuf) -> Self {
        Self { config, workspace_dir }
    }

    /// Read HEARTBEAT.md and return all parsed tasks.
    pub async fn collect_tasks(&self) -> Result<Vec<String>> {
        let heartbeat_path = self.workspace_dir.join("HEARTBEAT.md");
        if !heartbeat_path.exists() {
            return Ok(Vec::new());
        }
        let content = tokio::fs::read_to_string(&heartbeat_path).await?;
        Ok(Self::parse_tasks(&content))
    }

    /// Read HEARTBEAT.md and return executable agent prompts for each parsed task.
    pub async fn collect_task_prompts(&self) -> Result<Vec<String>> {
        Ok(self
            .collect_tasks()
            .await?
            .into_iter()
            .map(|task| self.task_prompt(&task))
            .collect())
    }

    /// Build the agent prompt for a single heartbeat task.
    pub fn task_prompt(&self, task: &str) -> String {
        format!("{}\n\n[Heartbeat Task] {task}", self.config.prompt)
    }

    /// Reconcile HEARTBEAT.md bullets into stable recurring Xin tasks. The
    /// content-derived name preserves identity across reordering and restart;
    /// removed bullets are disabled rather than deleted.
    pub async fn materialize_xin_tasks(&self, config: &Config) -> Result<Vec<XinTask>> {
        let tasks = self.collect_tasks().await?;
        let interval_secs = u64::from(self.config.interval_minutes.max(5)).saturating_mul(60);
        let definitions = tasks
            .iter()
            .map(|task| self.task_definition(task, interval_secs))
            .collect::<Vec<_>>();
        let keep_names = definitions.iter().map(|task| task.name.clone()).collect::<HashSet<_>>();

        let mut materialized = Vec::with_capacity(definitions.len());
        for definition in &definitions {
            let existing = crate::xin::store::ensure_system_task(config, definition)?;
            let needs_update = existing.description != definition.description
                || existing.priority != definition.priority
                || existing.payload != definition.payload
                || existing.interval_secs != definition.interval_secs
                || existing.max_failures != definition.max_failures
                || !existing.enabled;
            let task = if needs_update {
                crate::xin::store::update_task(
                    config,
                    &existing.id,
                    &XinTaskPatch {
                        description: definition.description.clone(),
                        priority: Some(definition.priority),
                        payload: Some(definition.payload.clone()),
                        interval_secs: Some(definition.interval_secs),
                        enabled: Some(true),
                        max_failures: Some(definition.max_failures),
                        approval_grant_json: definition.approval_grant_json.clone(),
                        ..XinTaskPatch::default()
                    },
                )?
            } else {
                existing
            };
            crate::xin::store::make_task_due_if_never_run(config, &task.id)?;
            materialized.push(crate::xin::store::get_task(config, &task.id)?);
        }

        crate::xin::store::disable_system_tasks_except(config, HEARTBEAT_TASK_PREFIX, &keep_names)?;
        Ok(materialized)
    }

    fn task_definition(&self, task: &str, interval_secs: u64) -> NewXinTask {
        NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: Self::stable_task_name(task),
            description: Some(format!("Materialized from HEARTBEAT.md: {task}")),
            kind: TaskKind::System,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::AgentSession,
            payload: self.task_prompt(task),
            recurring: true,
            interval_secs,
            max_failures: 10,
            approval_grant_json: None,
        }
    }

    fn stable_task_name(task: &str) -> String {
        let digest = Sha256::digest(task.as_bytes());
        format!("{HEARTBEAT_TASK_PREFIX}{}", hex::encode(digest))
    }

    pub fn is_materialized_task(task: &XinTask) -> bool {
        task.kind == TaskKind::System && task.name.starts_with(HEARTBEAT_TASK_PREFIX)
    }

    /// Parse tasks from HEARTBEAT.md (lines starting with `- `)
    fn parse_tasks(content: &str) -> Vec<String> {
        content
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                trimmed.strip_prefix("- ").map(ToString::to_string)
            })
            .collect()
    }

    /// Returns true when the provided local hour is inside the configured active window.
    pub fn is_within_active_hours(config: &HeartbeatConfig, hour: u8) -> bool {
        let Some((start, end)) = config
            .active_hours
            .get(0)
            .copied()
            .zip(config.active_hours.get(1).copied())
        else {
            return true;
        };
        let start = start.min(23);
        let end = end.min(23);
        if start <= end {
            hour >= start && hour <= end
        } else {
            hour >= start || hour <= end
        }
    }

    /// Create a default HEARTBEAT.md if it doesn't exist
    pub async fn ensure_heartbeat_file(workspace_dir: &Path) -> Result<()> {
        let path = workspace_dir.join("HEARTBEAT.md");
        if !path.exists() {
            let default = "# Periodic Tasks\n\n\
                           # Add tasks below (one per line, starting with `- `)\n\
                           # The agent will check this file on each heartbeat tick.\n\
                           #\n\
                           # Examples:\n\
                           # - Check my email for important messages\n\
                           # - Review my calendar for upcoming events\n\
                           # - Check the weather forecast\n";
            tokio::fs::write(&path, default).await?;
        }
        Ok(())
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tasks_basic() {
        let content = "# Tasks\n\n- Check email\n- Review calendar\nNot a task\n- Third task";
        let tasks = HeartbeatEngine::parse_tasks(content);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0], "Check email");
        assert_eq!(tasks[1], "Review calendar");
        assert_eq!(tasks[2], "Third task");
    }

    #[test]
    fn parse_tasks_empty_content() {
        assert!(HeartbeatEngine::parse_tasks("").is_empty());
    }

    #[test]
    fn parse_tasks_only_comments() {
        let tasks = HeartbeatEngine::parse_tasks("# No tasks here\n\nJust comments\n# Another");
        assert!(tasks.is_empty());
    }

    #[test]
    fn parse_tasks_with_leading_whitespace() {
        let content = "  - Indented task\n\t- Tab indented";
        let tasks = HeartbeatEngine::parse_tasks(content);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0], "Indented task");
        assert_eq!(tasks[1], "Tab indented");
    }

    #[test]
    fn parse_tasks_dash_without_space_ignored() {
        let content = "- Real task\n-\n- Another";
        let tasks = HeartbeatEngine::parse_tasks(content);
        // "-" trimmed = "-", does NOT start with "- " => skipped
        // "- Real task" => "Real task"
        // "- Another" => "Another"
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0], "Real task");
        assert_eq!(tasks[1], "Another");
    }

    #[test]
    fn parse_tasks_trailing_space_bullet_trimmed_to_dash() {
        // "- " trimmed becomes "-" (trim removes trailing space)
        // "-" does NOT start with "- " => skipped
        let content = "- ";
        let tasks = HeartbeatEngine::parse_tasks(content);
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn parse_tasks_bullet_with_content_after_spaces() {
        // "- hello  " trimmed becomes "- hello" => starts_with "- " => "hello"
        let content = "- hello  ";
        let tasks = HeartbeatEngine::parse_tasks(content);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0], "hello");
    }

    #[test]
    fn parse_tasks_unicode() {
        let content = "- Check email 📧\n- Review calendar 📅\n- 日本語タスク";
        let tasks = HeartbeatEngine::parse_tasks(content);
        assert_eq!(tasks.len(), 3);
        assert!(tasks[0].contains("📧"));
        assert!(tasks[2].contains("日本語"));
    }

    #[test]
    fn parse_tasks_mixed_markdown() {
        let content =
            "# Periodic Tasks\n\n## Quick\n- Task A\n\n## Long\n- Task B\n\n* Not a dash bullet\n1. Not numbered";
        let tasks = HeartbeatEngine::parse_tasks(content);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0], "Task A");
        assert_eq!(tasks[1], "Task B");
    }

    #[test]
    fn parse_tasks_single_task() {
        let tasks = HeartbeatEngine::parse_tasks("- Only one");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0], "Only one");
    }

    #[test]
    fn parse_tasks_many_tasks() {
        let content: String = (0..100).fold(String::new(), |mut s, i| {
            use std::fmt::Write;
            let _ = writeln!(s, "- Task {i}");
            s
        });
        let tasks = HeartbeatEngine::parse_tasks(&content);
        assert_eq!(tasks.len(), 100);
        assert_eq!(tasks[99], "Task 99");
    }

    #[test]
    fn active_hours_inclusive_range() {
        let config = HeartbeatConfig {
            interval_minutes: 30,
            active_hours: vec![8, 23],
            prompt: "p".to_string(),
        };
        assert!(HeartbeatEngine::is_within_active_hours(&config, 8));
        assert!(HeartbeatEngine::is_within_active_hours(&config, 23));
        assert!(!HeartbeatEngine::is_within_active_hours(&config, 7));
    }

    #[test]
    fn active_hours_wraparound_range() {
        let config = HeartbeatConfig {
            interval_minutes: 30,
            active_hours: vec![22, 6],
            prompt: "p".to_string(),
        };
        assert!(HeartbeatEngine::is_within_active_hours(&config, 23));
        assert!(HeartbeatEngine::is_within_active_hours(&config, 2));
        assert!(!HeartbeatEngine::is_within_active_hours(&config, 12));
    }

    #[test]
    fn active_hours_empty_allows_all_hours() {
        let config = HeartbeatConfig {
            interval_minutes: 30,
            active_hours: Vec::new(),
            prompt: "p".to_string(),
        };
        assert!(HeartbeatEngine::is_within_active_hours(&config, 0));
        assert!(HeartbeatEngine::is_within_active_hours(&config, 12));
        assert!(HeartbeatEngine::is_within_active_hours(&config, 23));
    }

    #[test]
    fn active_hours_out_of_range_hour_is_rejected() {
        let config = HeartbeatConfig {
            interval_minutes: 30,
            active_hours: vec![8, 23],
            prompt: "p".to_string(),
        };
        assert!(!HeartbeatEngine::is_within_active_hours(&config, 25));
    }

    #[tokio::test]
    async fn ensure_heartbeat_file_creates_file() {
        let dir = std::env::temp_dir().join("openprx_test_heartbeat");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        HeartbeatEngine::ensure_heartbeat_file(&dir).await.unwrap();

        let path = dir.join("HEARTBEAT.md");
        assert!(path.exists());
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("Periodic Tasks"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn ensure_heartbeat_file_does_not_overwrite() {
        let dir = std::env::temp_dir().join("openprx_test_heartbeat_no_overwrite");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let path = dir.join("HEARTBEAT.md");
        tokio::fs::write(&path, "- My custom task").await.unwrap();

        HeartbeatEngine::ensure_heartbeat_file(&dir).await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "- My custom task");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn collect_task_prompts_uses_config_prompt() {
        let dir = std::env::temp_dir().join("openprx_test_heartbeat_prompts");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        tokio::fs::write(dir.join("HEARTBEAT.md"), "- Check queue\n- Send digest")
            .await
            .unwrap();

        let engine = HeartbeatEngine::new(
            HeartbeatConfig {
                interval_minutes: 30,
                prompt: "Use the configured heartbeat prompt.".to_string(),
                ..HeartbeatConfig::default()
            },
            dir.clone(),
        );

        let prompts = engine.collect_task_prompts().await.unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].contains("Use the configured heartbeat prompt."));
        assert!(prompts[0].contains("[Heartbeat Task] Check queue"));
        assert!(prompts[1].contains("[Heartbeat Task] Send digest"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn materialization_is_stable_updates_in_place_and_disables_removed_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config {
            workspace_dir: temp.path().to_path_buf(),
            config_path: temp.path().join("config.toml"),
            ..Config::default()
        };
        config.heartbeat.interval_minutes = 30;
        config.heartbeat.prompt = "Heartbeat prompt v1".to_string();
        tokio::fs::write(temp.path().join("HEARTBEAT.md"), "- Check queue\n- Send digest")
            .await
            .unwrap();

        let first_engine = HeartbeatEngine::new(config.heartbeat.clone(), temp.path().to_path_buf());
        let first = first_engine.materialize_xin_tasks(&config).await.unwrap();
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|task| task.recurring && task.enabled));
        assert!(first.iter().all(|task| task.interval_secs == 1_800));
        assert!(first.iter().all(|task| task.next_run_at <= chrono::Utc::now()));
        let first_ids = first
            .iter()
            .map(|task| (task.name.clone(), task.id.clone()))
            .collect::<std::collections::HashMap<_, _>>();
        let events_before_replay = first
            .iter()
            .map(|task| crate::xin::store::list_task_events(&config, &task.id).unwrap().len())
            .sum::<usize>();
        let replay = first_engine.materialize_xin_tasks(&config).await.unwrap();
        assert_eq!(
            replay.iter().map(|task| &task.id).collect::<Vec<_>>(),
            first.iter().map(|task| &task.id).collect::<Vec<_>>()
        );
        let events_after_replay = replay
            .iter()
            .map(|task| crate::xin::store::list_task_events(&config, &task.id).unwrap().len())
            .sum::<usize>();
        assert_eq!(events_after_replay, events_before_replay);

        config.heartbeat.interval_minutes = 45;
        config.heartbeat.prompt = "Heartbeat prompt v2".to_string();
        tokio::fs::write(temp.path().join("HEARTBEAT.md"), "- Send digest\n- Check queue")
            .await
            .unwrap();
        let updated_engine = HeartbeatEngine::new(config.heartbeat.clone(), temp.path().to_path_buf());
        let updated = updated_engine.materialize_xin_tasks(&config).await.unwrap();
        assert_eq!(updated.len(), 2);
        for task in &updated {
            assert_eq!(first_ids.get(&task.name), Some(&task.id));
            assert_eq!(task.interval_secs, 2_700);
            assert!(task.payload.contains("Heartbeat prompt v2"));
        }

        tokio::fs::write(temp.path().join("HEARTBEAT.md"), "- Send digest")
            .await
            .unwrap();
        let retained = updated_engine.materialize_xin_tasks(&config).await.unwrap();
        assert_eq!(retained.len(), 1);
        let all = crate::xin::store::list_tasks(&config).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all.iter().filter(|task| task.enabled).count(), 1);
    }
}
