//! Production sources of the SL03 receipts at the runtime's canonical owners:
//! selection receipts for the skills a turn selected, and the one terminal
//! call-in shared by normal finalization and restart recovery.
//!
//! Everything here is best effort for the turn: a capture problem is logged
//! and never fails the turn that caused it.

use super::binding::{release_task_pins, task_pin};
use super::capture::{Correlation, ExperienceOutcome, TaskTerminal, on_task_terminal, task_key};
use super::store::SkillLifecycleStore;
use super::types::{LifecycleError, OwnerScope};
use std::path::Path;
use std::time::Duration;

/// How long the terminal call-in waits for the capture (its locks are bounded
/// to about a second on their own).
const TERMINAL_BUDGET: Duration = Duration::from_millis(1500);

/// The skills one turn selected and exposed, for one owner and task.
#[derive(Clone, Copy, Debug)]
pub struct SelectionRequest<'a> {
    pub workspace_dir: &'a Path,
    /// `[skills_lifecycle].enabled`; when `false` nothing is opened or written.
    pub enabled: bool,
    pub owner_id: &'a str,
    pub run_id: &'a str,
    pub task_id: Option<&'a str>,
    /// Names of the selected catalog skills.
    pub skill_names: &'a [&'a str],
}

/// Records a selection receipt for every selected skill that is managed for
/// the owner (an enabled active binding), keyed like hydration and the
/// terminal: the run id, else the task id. The task is pinned to the selected
/// revision, so the receipt, any later read and the experience agree. Calling
/// again for the same task and revision records nothing new.
///
/// Returns how many selected skills have a receipt after the call. A skill
/// that is not managed, a disabled lifecycle, a missing store or any capture
/// failure records nothing for that skill and never fails the caller.
/// Blocking; the capture locks are bounded.
pub fn record_selected_skills(request: &SelectionRequest<'_>) -> usize {
    if !request.enabled || request.skill_names.is_empty() {
        return 0;
    }
    let owner = match OwnerScope::new(request.owner_id) {
        Ok(owner) => owner,
        Err(error) => {
            tracing::warn!(%error, "skill selection receipts skipped: invalid owner scope");
            return 0;
        }
    };
    let store = match SkillLifecycleStore::open_existing(request.workspace_dir, owner.clone()) {
        Ok(Some(store)) => store,
        Ok(None) | Err(LifecycleError::OwnerMismatch { .. }) => return 0,
        Err(error) => {
            tracing::warn!(%error, "skill selection receipts skipped: the lifecycle store could not be opened");
            return 0;
        }
    };
    let Some(task) = task_key(request.task_id, Some(request.run_id), None) else {
        return 0;
    };
    let correlation = Correlation {
        run_id: Some(request.run_id.to_string()),
        task_id: request.task_id.map(str::to_string),
        ..Correlation::default()
    };
    let mut recorded = 0;
    for name in request.skill_names {
        let binding = match store.find_binding_by_name(name) {
            Ok(Some((binding, _))) if binding.enabled => binding,
            Ok(_) => continue,
            Err(error) => {
                tracing::warn!(%error, skill = *name, "skill selection receipt skipped: binding lookup failed");
                continue;
            }
        };
        let pin = match task_pin(&owner, &task, &binding.skill_id, || {
            store.pin_binding(&binding).map(Some)
        }) {
            Ok(Some(pin)) => pin,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, skill = *name, "skill selection receipt skipped: the revision could not be pinned");
                continue;
            }
        };
        match store.record_selection_once(&task, &pin, &correlation) {
            Ok(_) => recorded += 1,
            Err(error) => {
                tracing::warn!(%error, skill = *name, "failed to record a skill selection receipt");
            }
        }
    }
    recorded
}

/// How a turn ended, as the terminal owner observed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalKind {
    Completed,
    Silent,
    Failed,
    Cancelled,
}

/// What the terminal owner knows about a finished task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalObservation {
    pub kind: TerminalKind,
    /// The turn ended waiting for an approval rather than with an answer.
    pub awaiting_approval: bool,
    /// The facts behind the status (execution trace, provider outcome) were
    /// observed first hand or authenticated. Restart recovery without them
    /// reports `false`, and the join is `Unknown` rather than a claim.
    pub verified: bool,
    /// Reported by restart recovery. A terminal the live path already joined
    /// is kept as it is: the recovered view never replaces or conflicts with it.
    pub recovered: bool,
}

impl TerminalObservation {
    /// The outcome joined to the experience; `None` for a silent turn, which
    /// is not credited.
    #[must_use]
    pub const fn outcome(&self) -> Option<ExperienceOutcome> {
        match self.kind {
            TerminalKind::Silent => None,
            _ if !self.verified => Some(ExperienceOutcome::Unknown),
            TerminalKind::Completed if self.awaiting_approval => Some(ExperienceOutcome::Blocked),
            TerminalKind::Completed => Some(ExperienceOutcome::Success),
            TerminalKind::Failed => Some(ExperienceOutcome::Failure),
            TerminalKind::Cancelled => Some(ExperienceOutcome::Cancelled),
        }
    }
}

/// Task-terminal call-in of the skill lifecycle: joins the terminal to the
/// task's skill experience and releases the task's revision pins. Used by
/// normal finalization and by restart recovery, so both map the same way. A
/// silent turn is not credited (pins are released, nothing is joined).
/// Failures and a missed time budget are logged and never fail the caller.
pub async fn settle_task_terminal(
    workspace_dir: &Path,
    owner: Option<String>,
    task_key: Option<String>,
    terminal_id: &str,
    observation: TerminalObservation,
) {
    let (Some(owner), Some(task)) = (owner, task_key) else {
        return;
    };
    let owner = match OwnerScope::new(&owner) {
        Ok(owner) => owner,
        Err(error) => {
            tracing::warn!(%error, "skill lifecycle terminal skipped: invalid owner scope");
            return;
        }
    };
    let outcome = observation.outcome();
    let recovered = observation.recovered;
    let workspace = workspace_dir.to_path_buf();
    let terminal_id = terminal_id.to_string();
    let call_in = tokio::task::spawn_blocking(move || {
        let Some(outcome) = outcome else {
            release_task_pins(&owner, &task);
            return Ok(());
        };
        if recovered && already_joined(&workspace, &owner, &task) {
            release_task_pins(&owner, &task);
            return Ok(());
        }
        on_task_terminal(
            &workspace,
            &owner,
            &TaskTerminal {
                task_id: &task,
                terminal_receipt_id: &terminal_id,
                outcome,
            },
        )
        .map(|_| ())
    });
    match tokio::time::timeout(TERMINAL_BUDGET, call_in).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => tracing::warn!(%error, "skill lifecycle terminal join failed"),
        Ok(Err(error)) => tracing::warn!(%error, "skill lifecycle terminal task failed"),
        Err(_) => tracing::warn!("skill lifecycle terminal join exceeded its time budget; continuing"),
    }
}

/// Whether the task's experience already has a terminal (first join wins).
fn already_joined(workspace: &Path, owner: &OwnerScope, task: &str) -> bool {
    matches!(
        SkillLifecycleStore::open_existing(workspace, owner.clone()),
        Ok(Some(store)) if matches!(store.read_experience(task), Ok(Some(experience)) if experience.terminal.is_some())
    )
}
