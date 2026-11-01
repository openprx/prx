//! Production sources of the SL03 receipts at the runtime's canonical owners:
//! selection receipts for the skills a turn selected, and the one terminal
//! call-in shared by normal finalization and restart recovery.
//!
//! Everything here is best effort for the turn: a capture problem is logged
//! and never fails the turn that caused it.

use super::binding::{release_task_pins, task_pin};
use super::capture::{Correlation, ExperienceOutcome, TaskTerminal, on_task_terminal, task_key};
use super::facts::TerminalObservations;
use super::store::SkillLifecycleStore;
use super::types::{LifecycleError, OwnerScope};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// How long a background receipt worker may sit queued before the write is
/// abandoned. Queue time is not part of [`SELECTION_BUDGET`] or
/// [`TERMINAL_BUDGET`].
const WORKER_START_CAP: Duration = Duration::from_secs(60);

#[cfg(test)]
static RECEIPT_HOLD_BEFORE_START_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static RECEIPT_HOLD_AFTER_START_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
use std::sync::atomic::AtomicU64;

enum WorkerBudgetMiss {
    NeverStarted,
    Exceeded,
    Failed,
}

/// Runs `work` on the blocking pool. [`SELECTION_BUDGET`] / [`TERMINAL_BUDGET`]
/// start when the worker begins `work`, after it has left the queue.
async fn run_bounded_blocking<T, F>(budget: Duration, work: F) -> Result<T, WorkerBudgetMiss>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let started = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let flag = Arc::clone(&started);
    let ping = Arc::clone(&notify);
    let task = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        hold_receipt_worker(true);
        flag.store(true, Ordering::Release);
        ping.notify_one();
        #[cfg(test)]
        hold_receipt_worker(false);
        work()
    });
    if !wait_until_worker_started(&started, &notify).await {
        task.abort();
        return Err(WorkerBudgetMiss::NeverStarted);
    }
    match tokio::time::timeout(budget, task).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(WorkerBudgetMiss::Failed),
        Err(_) => Err(WorkerBudgetMiss::Exceeded),
    }
}

#[cfg(test)]
fn hold_receipt_worker(before_start: bool) {
    let millis = if before_start {
        RECEIPT_HOLD_BEFORE_START_MS.load(Ordering::SeqCst)
    } else {
        RECEIPT_HOLD_AFTER_START_MS.load(Ordering::SeqCst)
    };
    if millis > 0 {
        std::thread::sleep(Duration::from_millis(millis));
    }
}

/// Holds every receipt worker for `millis` before it reports that it started.
/// The guard clears the hold on drop.
#[cfg(test)]
pub(crate) struct ReceiptWorkerHold {
    before_start: bool,
}

#[cfg(test)]
impl ReceiptWorkerHold {
    pub(crate) fn before_start(millis: u64) -> Self {
        RECEIPT_HOLD_BEFORE_START_MS.store(millis, Ordering::SeqCst);
        Self { before_start: true }
    }

    pub(crate) fn after_start(millis: u64) -> Self {
        RECEIPT_HOLD_AFTER_START_MS.store(millis, Ordering::SeqCst);
        Self { before_start: false }
    }
}

#[cfg(test)]
impl Drop for ReceiptWorkerHold {
    fn drop(&mut self) {
        if self.before_start {
            RECEIPT_HOLD_BEFORE_START_MS.store(0, Ordering::SeqCst);
        } else {
            RECEIPT_HOLD_AFTER_START_MS.store(0, Ordering::SeqCst);
        }
    }
}

async fn wait_until_worker_started(started: &AtomicBool, notify: &Notify) -> bool {
    let cap = tokio::time::sleep(WORKER_START_CAP);
    tokio::pin!(cap);
    loop {
        if started.load(Ordering::Acquire) {
            return true;
        }
        let ping = notify.notified();
        if started.load(Ordering::Acquire) {
            return true;
        }
        tokio::select! {
            biased;
            () = ping => {}
            () = &mut cap => return false,
        }
    }
}

#[cfg(test)]
static LIFECYCLE_ACTIVITY: std::sync::LazyLock<Notify> = std::sync::LazyLock::new(Notify::new);

#[cfg(test)]
pub(crate) fn notify_lifecycle_activity() {
    LIFECYCLE_ACTIVITY.notify_waiters();
}

#[cfg(test)]
pub(crate) fn lifecycle_activity() -> &'static Notify {
    &LIFECYCLE_ACTIVITY
}

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
            Ok(_) => {
                recorded += 1;
                #[cfg(test)]
                notify_lifecycle_activity();
            }
            Err(error) => {
                tracing::warn!(%error, skill = *name, "failed to record a skill selection receipt");
            }
        }
    }
    recorded
}

/// Upper bound on remembered (run, skill) selections per process.
const MAX_REMEMBERED_SELECTIONS: usize = 4096;
/// How long the background selection write may take before it is abandoned.
const SELECTION_BUDGET: Duration = Duration::from_millis(3000);

static SCHEDULED_SELECTIONS: std::sync::LazyLock<parking_lot::Mutex<std::collections::VecDeque<String>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::VecDeque::new()));

/// Claims `(workspace, owner, run, skill)` in memory; `false` when already claimed.
fn claim_selection(request: &SelectionRequest<'_>, name: &str) -> bool {
    let key = format!(
        "{}\0{}\0{}\0{}",
        request.workspace_dir.display(),
        request.owner_id,
        request.run_id,
        name
    );
    let mut seen = SCHEDULED_SELECTIONS.lock();
    if seen.contains(&key) {
        return false;
    }
    if seen.len() >= MAX_REMEMBERED_SELECTIONS {
        seen.pop_front();
    }
    seen.push_back(key);
    true
}

#[cfg(test)]
static BACKGROUND: std::sync::LazyLock<parking_lot::Mutex<Vec<(String, tokio::task::JoinHandle<()>)>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(Vec::new()));

/// Waits for the background lifecycle writes started for `key` (a run or
/// terminal id). Test hook: production never awaits them.
#[cfg(test)]
pub(crate) async fn drain_background_for_test(key: &str) {
    let handles: Vec<_> = {
        let mut pending = BACKGROUND.lock();
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending).into_iter().partition(|(k, _)| k == key);
        *pending = rest;
        mine.into_iter().map(|(_, handle)| handle).collect()
    };
    for handle in handles {
        let _ = handle.await;
    }
}

/// Waits for every background lifecycle write this process has tracked.
#[cfg(test)]
pub(crate) async fn drain_all_background_for_test() {
    let handles: Vec<_> = {
        let mut pending = BACKGROUND.lock();
        std::mem::take(&mut *pending)
            .into_iter()
            .map(|(_, handle)| handle)
            .collect()
    };
    for handle in handles {
        let _ = handle.await;
    }
}

#[cfg(test)]
fn track_background(key: &str, handle: tokio::task::JoinHandle<()>) {
    BACKGROUND.lock().push((key.to_string(), handle));
}

#[cfg(not(test))]
fn track_background(_key: &str, handle: tokio::task::JoinHandle<()>) {
    drop(handle);
}

/// Turn-path entry for selection receipts. Never blocks and never fails the
/// turn: skills already claimed for this run are skipped from memory without
/// touching the store, and the store write for the rest runs in a spawned
/// blocking task. [`SELECTION_BUDGET`] starts when that worker begins the
/// write; time spent queued is not part of the budget (errors are logged).
/// Outside a tokio
/// runtime the receipts are skipped with a warning. Returns how many skills
/// were handed to the background writer.
pub fn schedule_selection_receipts(request: &SelectionRequest<'_>) -> usize {
    if !request.enabled || request.skill_names.is_empty() {
        return 0;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("skill selection receipts skipped: no async runtime");
        return 0;
    };
    let fresh: Vec<String> = request
        .skill_names
        .iter()
        .filter(|name| claim_selection(request, name))
        .map(|name| (*name).to_string())
        .collect();
    if fresh.is_empty() {
        return 0;
    }
    let count = fresh.len();
    let workspace = request.workspace_dir.to_path_buf();
    let owner = request.owner_id.to_string();
    let run = request.run_id.to_string();
    let task = request.task_id.map(str::to_string);
    let key = run.clone();
    let dropped = (
        workspace.clone(),
        owner.clone(),
        task_key(request.task_id, Some(request.run_id), None),
    );
    let handle = runtime.spawn(async move {
        let outcome = run_bounded_blocking(SELECTION_BUDGET, move || {
            let names: Vec<&str> = fresh.iter().map(String::as_str).collect();
            record_selected_skills(&SelectionRequest {
                workspace_dir: &workspace,
                enabled: true,
                owner_id: &owner,
                run_id: &run,
                task_id: task.as_deref(),
                skill_names: &names,
            })
        })
        .await;
        match outcome {
            Ok(_) => {}
            Err(WorkerBudgetMiss::Failed) => {
                tracing::warn!("skill selection receipt task failed");
                mark_selection_dropped(&dropped);
            }
            Err(WorkerBudgetMiss::NeverStarted | WorkerBudgetMiss::Exceeded) => {
                tracing::warn!("skill selection receipts exceeded their time budget; continuing");
                mark_selection_dropped(&dropped);
            }
        }
    });
    track_background(&key, handle);
    count
}

/// A selection write that failed or timed out leaves a task that selected a
/// skill without a receipt. Marking its experience as having dropped evidence
/// keeps it from ever counting as work that no skill covered.
fn mark_selection_dropped((workspace, owner, task): &(std::path::PathBuf, String, Option<String>)) {
    let Some(task) = task else {
        return;
    };
    let Ok(owner) = OwnerScope::new(owner) else {
        return;
    };
    match SkillLifecycleStore::open_existing(workspace, owner) {
        Ok(Some(store)) => store.mark_dropped_evidence(task),
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "could not mark a dropped selection receipt"),
    }
}

/// Starts [`settle_task_terminal`] without making the caller wait for it:
/// restart recovery must not extend its own deadline. Outside a tokio runtime
/// it is skipped with a warning.
pub fn spawn_settle_task_terminal(
    workspace_dir: std::path::PathBuf,
    owner: Option<String>,
    task_key: Option<String>,
    terminal_id: String,
    observation: TerminalObservation,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("skill lifecycle terminal skipped: no async runtime");
        return;
    };
    let key = terminal_id.clone();
    let handle = runtime.spawn(async move {
        // Recovery has no first-hand trace, so no observations: adherence is Unknown.
        settle_task_terminal(&workspace_dir, owner, task_key, &terminal_id, observation, None).await;
    });
    track_background(&key, handle);
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
    /// The task is a root task (it has no parent run). Only root tasks leave a
    /// record of work that used no managed skill.
    pub root: bool,
}

impl TerminalObservation {
    /// The outcome joined to the experience; `None` for a silent turn, which
    /// is not credited. Unverified facts (including recovered Failed/Cancelled
    /// runs) are `Unknown` by ruling: conservative, never learned from.
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
    observations: Option<TerminalObservations>,
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
    // Only an enabled lifecycle records work that used no managed skill.
    let root = observation.root && crate::skills::lifecycle_runtime::lifecycle_enabled_for(workspace_dir);
    let workspace = workspace_dir.to_path_buf();
    let terminal_id = terminal_id.to_string();
    let call_in = run_bounded_blocking(TERMINAL_BUDGET, move || {
        let Some(outcome) = outcome else {
            release_task_pins(&owner, &task);
            return Ok(());
        };
        if recovered && already_joined(&workspace, &owner, &task) {
            release_task_pins(&owner, &task);
            return Ok(());
        }
        let joined = on_task_terminal(
            &workspace,
            &owner,
            &TaskTerminal {
                task_id: &task,
                terminal_receipt_id: &terminal_id,
                outcome,
                observations: observations.as_ref(),
                root_task: root,
            },
        )
        .map(|_| ());
        if joined.is_ok() {
            #[cfg(test)]
            notify_lifecycle_activity();
        }
        joined
    })
    .await;
    match call_in {
        Ok(Ok(())) => {}
        // A join that already exists with another terminal is expected on a
        // replay racing the live path; the first join is kept.
        Ok(Err(error @ LifecycleError::TerminalConflict { .. })) => {
            tracing::debug!(%error, "skill lifecycle terminal already joined; keeping the first join");
        }
        Ok(Err(error)) => tracing::warn!(%error, "skill lifecycle terminal join failed"),
        Err(WorkerBudgetMiss::Failed) => tracing::warn!("skill lifecycle terminal task failed"),
        Err(WorkerBudgetMiss::NeverStarted | WorkerBudgetMiss::Exceeded) => {
            tracing::warn!("skill lifecycle terminal join exceeded its time budget; continuing")
        }
    }
}

/// Whether the task's experience already has a terminal (first join wins).
fn already_joined(workspace: &Path, owner: &OwnerScope, task: &str) -> bool {
    matches!(
        SkillLifecycleStore::open_existing(workspace, owner.clone()),
        Ok(Some(store)) if matches!(store.read_experience(task), Ok(Some(experience)) if experience.terminal.is_some())
    )
}
