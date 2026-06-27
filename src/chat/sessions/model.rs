//! Chat-side view projection of a child TUI session.
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

/// What kind of child TUI surface this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedKind {
    /// A background agent session (`/bg`, or a model-spawned sub-agent).
    Agent,
    /// A background shell session (v2; not produced in v1a).
    Shell,
    /// An interactive PTY shell session (v3; `/pty`).
    Pty,
    /// Read-only conversation transcript viewer (`Ctrl+O`).
    Transcript,
    /// Foreground tool approval prompt.
    Approval,
}

impl ManagedKind {
    /// Stable lowercase label for display.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Shell => "shell",
            Self::Pty => "pty",
            Self::Transcript => "transcript",
            Self::Approval => "approval",
        }
    }
}

/// Who initiated a child session (v5, §17 unification).
///
/// Both user-initiated `/bg`/`/shell`/`/pty` sessions and model-initiated
/// sub-agents (the LLM calling `sessions_spawn` mid-turn) share the *same*
/// registry and the *same* list / switcher; this marker only distinguishes their
/// provenance for display so the operator can tell at a glance which sessions the
/// model started for itself.
///
/// The discriminator is `SubAgentRun.parent_run_id`: a user `/bg` is invoked
/// directly with no spawn-execution context (`None`), whereas a model-spawned
/// sub-agent inherits the per-turn run id as its `parent_run_id` (`Some`).
/// Shells and PTYs are always operator-initiated, so they are always `User`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOrigin {
    /// Started by the operator (`/bg`, `/shell`, `/pty`).
    User,
    /// Started by the model itself via a `sessions_spawn` tool call mid-turn.
    Model,
}

impl SessionOrigin {
    /// Stable lowercase label for display.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Model => "model",
        }
    }

    /// Infer the origin of an agent run from its `parent_run_id` (see the type
    /// docs): a child run created by the model mid-turn carries the per-turn run
    /// id as parent; a top-level operator `/bg` has none.
    #[must_use]
    pub const fn from_parent_run_id(parent_run_id: Option<&String>) -> Self {
        if parent_run_id.is_some() {
            Self::Model
        } else {
            Self::User
        }
    }
}

/// UI-facing status of a managed session.
///
/// `NeedsInput` is produced by [`project_status`] when a background sub-agent
/// suspends awaiting an operator approval decision
/// ([`SubAgentStatus::AwaitingInput`]).
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

/// Minimal chat-side view of a single child TUI session.
#[derive(Debug, Clone)]
pub struct ManagedSessionView {
    pub id: SessionId,
    /// Display-only short alias `#N`.
    pub seq: u64,
    pub kind: ManagedKind,
    /// Who initiated the session (v5): operator vs model.
    pub origin: SessionOrigin,
    /// Task / command text (already trimmed by the source).
    pub title: String,
    pub status: ManagedStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Persisted summary of a child TUI session that ran during a chat session
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
    /// Provenance label (`user` / `model`), so reload recap can still
    /// distinguish operator-initiated sessions from model-spawned sub-agents
    /// (v5). Stored as a stable lowercase string (the [`SessionOrigin::as_str`]
    /// vocabulary) for the same format-decoupling reason as `kind` / `status`.
    ///
    /// `#[serde(default)]` keeps pre-v5 persisted blobs (which lack this field)
    /// loadable: a missing `origin` defaults to `"user"`, the conservative
    /// assumption for legacy summaries (operator-initiated).
    #[serde(default = "default_origin")]
    pub origin: String,
    /// Final status label. One of `completed` / `failed` / `cancelled` /
    /// `interrupted` (the latter is the v4 sentinel for a session that was
    /// still `running` when the chat session was persisted).
    pub status: String,
    /// Task text (agent) or command line (shell/pty).
    pub title: String,
    /// Completion / failure summary body recorded by the run (may be empty).
    #[serde(default)]
    pub summary: String,
    /// When the child session started.
    pub created_at: DateTime<Utc>,
}

/// The v4 sentinel status for a child session that was still `running` when
/// its owning chat session was persisted. Reload presents it as a terminal,
/// non-revivable state (the live process is gone).
pub const STATUS_INTERRUPTED: &str = "interrupted";

/// Serde default for [`PersistedSessionSummary::origin`]: a pre-v5 blob without
/// the field is treated as operator-initiated (`"user"`), the conservative
/// assumption matching the legacy behaviour where only `/bg`/`/shell`/`/pty`
/// existed.
fn default_origin() -> String {
    SessionOrigin::User.as_str().to_string()
}

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
            origin: view.origin.as_str().to_string(),
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
        // A run suspended awaiting an operator approval decision surfaces as
        // NeedsInput (the `❓` glyph + status-line counter), the reversible
        // non-terminal state that drives the chat approval UX.
        SubAgentStatus::AwaitingInput { .. } => ManagedStatus::NeedsInput,
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
        origin: SessionOrigin::from_parent_run_id(run.parent_run_id.as_ref()),
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
        // Shells are always operator-initiated (`/shell`); the model has no
        // shell-spawn path.
        origin: SessionOrigin::User,
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
        // PTYs are always operator-initiated (`/pty`); the model has no
        // interactive-PTY spawn path.
        origin: SessionOrigin::User,
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
    fn origin_user_when_no_parent_run() {
        // A top-level operator `/bg` has no spawn-execution context, so the
        // registry run carries no parent_run_id → User origin.
        assert_eq!(SessionOrigin::from_parent_run_id(None), SessionOrigin::User);
        assert_eq!(SessionOrigin::User.as_str(), "user");
    }

    #[test]
    fn origin_model_when_parent_run_present() {
        // A model-spawned sub-agent inherits the per-turn run id as its
        // parent_run_id → Model origin.
        let parent = "turn-run-123".to_string();
        assert_eq!(SessionOrigin::from_parent_run_id(Some(&parent)), SessionOrigin::Model);
        assert_eq!(SessionOrigin::Model.as_str(), "model");
    }

    #[test]
    fn project_run_infers_origin_from_parent() {
        let mut run = SubAgentRun {
            id: "child".into(),
            task: "do thing".into(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            status: SubAgentStatus::Running,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            history: std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "scope".into(),
            spawn_depth: 0,
        };
        assert_eq!(project_run(&run, 1).origin, SessionOrigin::User);
        run.parent_run_id = Some("turn-1".into());
        assert_eq!(project_run(&run, 1).origin, SessionOrigin::Model);
    }

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
            origin: SessionOrigin::User,
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

    fn view_with_origin(origin: SessionOrigin) -> ManagedSessionView {
        ManagedSessionView {
            id: SessionId::from_run_id("run-o"),
            seq: 7,
            kind: ManagedKind::Agent,
            origin,
            title: "model child".to_string(),
            status: ManagedStatus::Completed,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn from_view_carries_origin_label() {
        // Bug-V5-2: from_view must persist the view's provenance so reload recap
        // can still distinguish model-spawned from operator-initiated sessions.
        let model = PersistedSessionSummary::from_view(&view_with_origin(SessionOrigin::Model), "done");
        assert_eq!(model.origin, "model");
        let user = PersistedSessionSummary::from_view(&view_with_origin(SessionOrigin::User), "done");
        assert_eq!(user.origin, "user");
    }

    #[test]
    fn persisted_summary_origin_round_trip() {
        // Bug-V5-2: a model-origin summary survives a serialize/deserialize cycle.
        let original = PersistedSessionSummary::from_view(&view_with_origin(SessionOrigin::Model), "done");
        assert_eq!(original.origin, "model");
        let json = serde_json::to_string(&original).expect("test: serialize");
        let restored: PersistedSessionSummary = serde_json::from_str(&json).expect("test: deserialize");
        assert_eq!(restored, original);
        assert_eq!(restored.origin, "model");
    }

    #[test]
    fn persisted_summary_legacy_blob_without_origin_defaults_to_user() {
        // Bug-V5-2 backward compat: a pre-v5 persisted blob has no `origin` field.
        // It must deserialize (not error) and default the field to "user".
        let legacy = r#"{
            "id": "run-legacy",
            "seq": 1,
            "kind": "agent",
            "status": "completed",
            "title": "old task",
            "summary": "old body",
            "created_at": "2024-01-01T00:00:00Z"
        }"#;
        let restored: PersistedSessionSummary =
            serde_json::from_str(legacy).expect("test: legacy blob without origin must still deserialize");
        assert_eq!(restored.origin, "user");
        assert_eq!(restored.id, "run-legacy");
        assert_eq!(restored.kind, "agent");
    }

    #[test]
    fn awaiting_input_projects_needs_input() {
        // The suspend-on-approval state is the sole source of NeedsInput.
        assert_eq!(
            project_status(&SubAgentStatus::AwaitingInput {
                prompt: "shell rm -rf".into()
            }),
            ManagedStatus::NeedsInput
        );
    }

    #[test]
    fn non_awaiting_states_never_project_needs_input() {
        // Every other underlying variant must avoid the NeedsInput bucket.
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
