//! `schedule_run_started` and `schedule_run_finished`: the notification pair
//! of one run of a cron job or Xin task.

use super::event::HookEvent;
use super::identity::HookIdentity;
use super::manager::HookManager;

/// Characters of a run's output the finished event carries.
const OUTPUT_EXCERPT_CHARS: usize = 200;

/// One claimed run of a scheduled job or task. [`Self::start`] queues the
/// started event; [`Self::finish`] queues the finished one. Both are queued
/// and never awaited, and delivery continues after this value is dropped.
pub struct ScheduleRun {
    hooks: HookManager,
    identity: HookIdentity,
    mode: &'static str,
    name: Option<String>,
    started: std::time::Instant,
}

impl ScheduleRun {
    /// Queue `schedule_run_started` for a run of the job or task `id` of the
    /// scheduler `kind` (`cron` or `xin`).
    pub async fn start(
        config: &crate::config::Config,
        kind: &'static str,
        id: &str,
        mode: &'static str,
        name: Option<&str>,
    ) -> Self {
        let run = Self {
            hooks: HookManager::for_config_snapshot(config),
            identity: HookIdentity::for_schedule(kind, id, uuid::Uuid::new_v4().to_string()),
            mode,
            name: name.map(str::to_string),
            started: std::time::Instant::now(),
        };
        run.hooks
            .emit_for(
                &run.identity,
                HookEvent::ScheduleRunStarted,
                serde_json::json!({"mode": run.mode, "name": run.name}),
            )
            .await;
        run
    }

    /// Queue `schedule_run_finished` with the run's `status` (`success`,
    /// `failed` or `cancelled`) and the masked start of its output.
    pub async fn finish(&self, status: &str, output: &str) {
        let masked = crate::security::redaction::redact_secrets(output);
        let excerpt = masked.chars().take(OUTPUT_EXCERPT_CHARS).collect::<String>();
        self.hooks
            .emit_for(
                &self.identity,
                HookEvent::ScheduleRunFinished,
                serde_json::json!({
                    "mode": self.mode,
                    "name": self.name,
                    "status": status,
                    "duration_ms": u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "output_excerpt": excerpt,
                }),
            )
            .await;
    }
}
