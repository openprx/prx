//! `child_started`, `child_finished` and `child_failed`: what the parent
//! session's owner is told about a child run (a delegate or a task-mode
//! `sessions_spawn` run). They are emitted under the parent turn's identity,
//! so a script ties the child to the turn that launched it.

use super::event::HookEvent;
use super::identity::notify;

/// Characters of a failure text the failed event carries.
const ERROR_EXCERPT_CHARS: usize = 200;

/// One child run the parent turn launched.
pub struct ChildRun {
    session: String,
    run: Option<String>,
    mode: &'static str,
    started: std::time::Instant,
}

impl ChildRun {
    /// Queue `child_started` for the child `session` (and `run`) opened in
    /// `mode` (`delegate` or `sessions_spawn`) for `agent`.
    #[must_use]
    pub fn started(
        session: &str,
        run: Option<&str>,
        mode: &'static str,
        agent: Option<&str>,
        brief_chars: usize,
    ) -> Self {
        notify(
            HookEvent::ChildStarted,
            serde_json::json!({
                "child_session": session,
                "child_run": run,
                "mode": mode,
                "agent": agent,
                "brief_chars": brief_chars,
            }),
        );
        Self {
            session: session.to_string(),
            run: run.map(str::to_string),
            mode,
            started: std::time::Instant::now(),
        }
    }

    fn duration_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Queue `child_finished`: the child committed a result of `result_chars`
    /// characters.
    pub fn finished(&self, result_chars: usize) {
        notify(
            HookEvent::ChildFinished,
            serde_json::json!({
                "child_session": self.session,
                "child_run": self.run,
                "mode": self.mode,
                "status": "completed",
                "duration_ms": self.duration_ms(),
                "result_chars": result_chars,
            }),
        );
    }

    /// Queue `child_failed`: the child failed, or was cancelled when
    /// `cancelled`. `error` is masked and cut before it is carried.
    pub fn failed(&self, cancelled: bool, error: &str) {
        let masked = crate::security::redaction::redact_secrets(error);
        notify(
            HookEvent::ChildFailed,
            serde_json::json!({
                "child_session": self.session,
                "child_run": self.run,
                "mode": self.mode,
                "status": if cancelled { "cancelled" } else { "failed" },
                "duration_ms": self.duration_ms(),
                "error": masked.chars().take(ERROR_EXCERPT_CHARS).collect::<String>(),
            }),
        );
    }
}
