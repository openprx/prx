//! Chat-side view projection of a background session.
//!
//! This is a *projection* of the live `sessions_spawn` registry entry
//! ([`crate::tools::sessions_spawn::SubAgentRun`]) onto the minimal shape the
//! chat UI needs. It deliberately does not copy the full run (history, steer
//! channel, abort handle stay in the registry); it only carries display fields.
//!
//! Status mapping rules (v1a):
//! - The underlying [`SubAgentStatus`] only has `Running` / `Completed` /
//!   `Failed`.
//! - `kill` records `Failed("killed by user")` in the registry, which this
//!   projection maps to [`ManagedStatus::Cancelled`].
//! - [`ManagedStatus::NeedsInput`] is reserved for the v1.1 event bridge and is
//!   **never** produced here (there is no underlying signal for it yet).

use super::id::SessionId;
use super::shell::{ShellSession, ShellStatus};
use crate::tools::sessions_spawn::{SubAgentRun, SubAgentStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Sentinel message written by `sessions_spawn` kill, projected to `Cancelled`.
const KILLED_BY_USER: &str = "killed by user";

/// What kind of session this is. v1a only ever produces `Agent`; `Shell` is
/// reserved for v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedKind {
    /// A background agent session (`/bg`, or a model-spawned sub-agent).
    Agent,
    /// A background shell session (v2; not produced in v1a).
    Shell,
    /// An interactive PTY shell session (v3; `/pty`).
    Pty,
}

impl ManagedKind {
    /// Stable lowercase label for display.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Shell => "shell",
            Self::Pty => "pty",
        }
    }
}

/// UI-facing status of a managed session.
///
/// `NeedsInput` is retained as a reserved variant for the v1.1 event bridge but
/// is never produced by [`project_status`] in v1a/v1b.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedStatus {
    Running,
    NeedsInput,
    Completed,
    Failed,
    Cancelled,
}

impl ManagedStatus {
    /// Stable lowercase label for display.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::NeedsInput => "needs-input",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Minimal chat-side view of a single background session.
#[derive(Debug, Clone)]
pub struct ManagedSessionView {
    pub id: SessionId,
    /// Display-only short alias `#N`.
    pub seq: u64,
    pub kind: ManagedKind,
    /// Task / command text (already trimmed by the source).
    pub title: String,
    pub status: ManagedStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Persisted summary of a background session that ran during a chat session
/// (v4).
///
/// This is the **durable** counterpart of [`ManagedSessionView`]: it carries
/// only the fields needed to *describe* a finished (or interrupted) background
/// session after the live process is long gone. It is serialized inside the
/// owning [`crate::chat::session::ChatSession`] blob and reloaded for display
/// only — reloading **never** revives a process, sub-agent, or PTY.
///
/// Status is stored as a stable lowercase string (the `ManagedStatus::as_str`
/// vocabulary plus the v4 terminal sentinel `"interrupted"`) rather than the
/// enum, so the persisted format stays decoupled from the in-memory enum and
/// tolerant of future variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSessionSummary {
    /// Underlying session id (run UUID for agents, shell/pty id otherwise).
    pub id: String,
    /// Display sequence `#N` it held during the live session.
    pub seq: u64,
    /// Session kind label (`agent` / `shell` / `pty`).
    pub kind: String,
    /// Final status label. One of `completed` / `failed` / `cancelled` /
    /// `interrupted` (the latter is the v4 sentinel for a session that was
    /// still `running` when the chat session was persisted).
    pub status: String,
    /// Task text (agent) or command line (shell/pty).
    pub title: String,
    /// Completion / failure summary body recorded by the run (may be empty).
    #[serde(default)]
    pub summary: String,
    /// When the background session started.
    pub created_at: DateTime<Utc>,
}

/// The v4 sentinel status for a background session that was still `running` when
/// its owning chat session was persisted. Reload presents it as a terminal,
/// non-revivable state (the live process is gone).
pub const STATUS_INTERRUPTED: &str = "interrupted";

impl PersistedSessionSummary {
    /// Build a persisted summary from a [`ManagedSessionView`] and an optional
    /// completion summary body, mapping a still-`Running` session to the
    /// terminal [`STATUS_INTERRUPTED`] sentinel (v4: never persist a live
    /// status that reload could mistake for an active process).
    #[must_use]
    pub fn from_view(view: &ManagedSessionView, summary: impl Into<String>) -> Self {
        let status = match view.status {
            // A session still running at persistence time can never be revived,
            // so it is recorded as a distinct terminal sentinel rather than
            // "running" / "needs-input".
            ManagedStatus::Running | ManagedStatus::NeedsInput => STATUS_INTERRUPTED.to_string(),
            terminal => terminal.as_str().to_string(),
        };
        Self {
            id: view.id.as_str().to_string(),
            seq: view.seq,
            kind: view.kind.as_str().to_string(),
            status,
            title: view.title.clone(),
            summary: summary.into(),
            created_at: view.created_at,
        }
    }
}

/// Project a registry [`SubAgentStatus`] onto the UI [`ManagedStatus`].
///
/// Pure function (no allocation, no lock) so it is trivially unit-testable. See
/// the module docs for the `Failed("killed by user") -> Cancelled` rule and the
/// v1a no-`NeedsInput` decision.
#[must_use]
pub fn project_status(status: &SubAgentStatus) -> ManagedStatus {
    match status {
        SubAgentStatus::Running => ManagedStatus::Running,
        SubAgentStatus::Completed(_) => ManagedStatus::Completed,
        SubAgentStatus::Failed(msg) if msg == KILLED_BY_USER => ManagedStatus::Cancelled,
        SubAgentStatus::Failed(_) => ManagedStatus::Failed,
    }
}

/// Project a single registry run onto a [`ManagedSessionView`], assigning the
/// supplied display sequence number.
///
/// `title` is truncated to a reasonable display width to keep the `/sessions`
/// list readable.
#[must_use]
pub fn project_run(run: &SubAgentRun, seq: u64) -> ManagedSessionView {
    const MAX_TITLE: usize = 80;
    let title = if run.task.chars().count() > MAX_TITLE {
        let truncated: String = run.task.chars().take(MAX_TITLE).collect();
        format!("{truncated}…")
    } else {
        run.task.clone()
    };
    ManagedSessionView {
        id: SessionId::from_run_id(&run.id),
        seq,
        kind: ManagedKind::Agent,
        title,
        status: project_status(&run.status),
        created_at: run.started_at,
        updated_at: run.started_at,
    }
}

/// Project a background [`ShellSession`]'s [`ShellStatus`] onto the unified UI
/// [`ManagedStatus`]. Pure function (no lock beyond the cheap status read in the
/// caller), trivially unit-testable.
#[must_use]
pub const fn project_shell_status(status: &ShellStatus) -> ManagedStatus {
    match status {
        ShellStatus::Running => ManagedStatus::Running,
        ShellStatus::Completed => ManagedStatus::Completed,
        ShellStatus::Failed(_) => ManagedStatus::Failed,
        ShellStatus::Cancelled => ManagedStatus::Cancelled,
    }
}

/// Project a background shell session onto a [`ManagedSessionView`] with the
/// supplied display sequence number, so `/sessions`, the switcher, and the
/// status line treat agents and shells uniformly (one seq space).
#[must_use]
pub fn project_shell(session: &ShellSession, seq: u64) -> ManagedSessionView {
    const MAX_TITLE: usize = 80;
    let title = if session.command.chars().count() > MAX_TITLE {
        let truncated: String = session.command.chars().take(MAX_TITLE).collect();
        format!("{truncated}…")
    } else {
        session.command.clone()
    };
    ManagedSessionView {
        id: session.id.clone(),
        seq,
        kind: ManagedKind::Shell,
        title,
        status: project_shell_status(&session.status()),
        created_at: session.started_at,
        updated_at: session.started_at,
    }
}

/// Project an interactive PTY shell session onto a [`ManagedSessionView`] with
/// the supplied display sequence number, so `/sessions` and `/kill` treat PTY
/// sessions in the same seq space as agents and background shells (v3a).
///
/// PTY sessions have only a binary liveness signal (`has_exited`); a live
/// session is `Running`, an exited one is `Completed` (we do not distinguish the
/// exit code here — the user saw the full interactive output during the
/// handoff).
#[cfg(feature = "terminal-tui")]
#[must_use]
pub fn project_pty(session: &super::pty::PtyShellSession, seq: u64) -> ManagedSessionView {
    const MAX_TITLE: usize = 80;
    let title = if session.command.chars().count() > MAX_TITLE {
        let truncated: String = session.command.chars().take(MAX_TITLE).collect();
        format!("{truncated}…")
    } else {
        session.command.clone()
    };
    let status = if session.has_exited() {
        ManagedStatus::Completed
    } else {
        ManagedStatus::Running
    };
    ManagedSessionView {
        id: session.id.clone(),
        seq,
        kind: ManagedKind::Pty,
        title,
        status,
        created_at: session.started_at,
        updated_at: session.started_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_maps_to_running() {
        assert_eq!(project_status(&SubAgentStatus::Running), ManagedStatus::Running);
    }

    #[test]
    fn completed_maps_to_completed() {
        assert_eq!(
            project_status(&SubAgentStatus::Completed("done".into())),
            ManagedStatus::Completed
        );
    }

    #[test]
    fn killed_by_user_maps_to_cancelled() {
        assert_eq!(
            project_status(&SubAgentStatus::Failed("killed by user".into())),
            ManagedStatus::Cancelled
        );
    }

    #[test]
    fn other_failure_maps_to_failed() {
        assert_eq!(
            project_status(&SubAgentStatus::Failed("boom".into())),
            ManagedStatus::Failed
        );
    }

    fn view_with_status(status: ManagedStatus) -> ManagedSessionView {
        ManagedSessionView {
            id: SessionId::from_run_id("run-x"),
            seq: 3,
            kind: ManagedKind::Agent,
            title: "build the report".to_string(),
            status,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn from_view_maps_running_to_interrupted() {
        // A session still running at persistence time can never be revived, so
        // it must be recorded as the terminal `interrupted` sentinel.
        let summary = PersistedSessionSummary::from_view(&view_with_status(ManagedStatus::Running), "");
        assert_eq!(summary.status, STATUS_INTERRUPTED);
        assert_eq!(summary.id, "run-x");
        assert_eq!(summary.seq, 3);
        assert_eq!(summary.kind, "agent");
        assert_eq!(summary.title, "build the report");
    }

    #[test]
    fn from_view_maps_needs_input_to_interrupted() {
        let summary = PersistedSessionSummary::from_view(&view_with_status(ManagedStatus::NeedsInput), "");
        assert_eq!(summary.status, STATUS_INTERRUPTED);
    }

    #[test]
    fn from_view_preserves_terminal_status_and_summary() {
        let summary = PersistedSessionSummary::from_view(&view_with_status(ManagedStatus::Completed), "result body");
        assert_eq!(summary.status, "completed");
        assert_eq!(summary.summary, "result body");

        let failed = PersistedSessionSummary::from_view(&view_with_status(ManagedStatus::Failed), "boom");
        assert_eq!(failed.status, "failed");

        let cancelled = PersistedSessionSummary::from_view(&view_with_status(ManagedStatus::Cancelled), "");
        assert_eq!(cancelled.status, "cancelled");
    }

    #[test]
    fn persisted_summary_serde_round_trip() {
        let original = PersistedSessionSummary::from_view(&view_with_status(ManagedStatus::Completed), "done");
        let json = serde_json::to_string(&original).expect("test: serialize");
        let restored: PersistedSessionSummary = serde_json::from_str(&json).expect("test: deserialize");
        assert_eq!(restored, original);
    }

    #[test]
    fn never_projects_needs_input() {
        // Exhaustively over the underlying variants: none yields NeedsInput.
        for status in [
            SubAgentStatus::Running,
            SubAgentStatus::Completed("x".into()),
            SubAgentStatus::Failed("y".into()),
            SubAgentStatus::Failed("killed by user".into()),
        ] {
            assert_ne!(project_status(&status), ManagedStatus::NeedsInput);
        }
    }
}
