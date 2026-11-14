//! Process-level projection of the recovery cost outbox (Part-2 D§5.2, OV-11)
//! and the one-time legacy decision file warning (D§1, OV-1).
//!
//! Every committed metered terminal has one outbox row. The inline path in
//! `agent::terminal::recovery_commit` projects it right after the commit; this
//! projector owns every row the inline path did not finish. It needs only a
//! recovery repository whose outbox table exists: it never activates the
//! recovery authority and never reads the recording settings, so rows written
//! while recording was on are projected after a restart with recording off.
use crate::config::schema::CostConfig;
use crate::llm::route_decision::MeteredTokenUsageRecord;
use crate::memory::Memory;
use crate::memory::recovery_authority::{RecoveryCostRow, RecoveryCostState, RecoveryPricing, RecoveryRepository};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Rows per keyset page.
const PAGE: usize = 64;
/// One pass stops at this deadline; the next pass restarts from the beginning.
/// It also bounds how long [`CostOutboxProjector::stop`] waits.
const PASS_DEADLINE: Duration = Duration::from_secs(15);

/// The projector's cadence (OV-11).
#[derive(Clone, Copy, Debug)]
struct Cadence {
    /// Interval while retryable rows remain and the last pass made progress.
    retry: Duration,
    /// Back-off ceiling after passes without progress.
    max: Duration,
}

impl Cadence {
    /// 60 s while retryable rows remain, doubling up to 15 min.
    const PROCESS: Self = Self {
        retry: Duration::from_secs(60),
        max: Duration::from_mins(15),
    };

    /// The wait after `report`, updating the back-off `interval`. `None`
    /// waits for an inline failure to re-arm: only a complete scan that left
    /// no retryable row is idle. A failed or deadline-cut pass is not
    /// exhaustion and is retried on the back-off.
    fn next_wait(self, report: &PassReport, interval: &mut Duration) -> Option<Duration> {
        if report.exhausted && report.retryable_left == 0 {
            *interval = self.retry;
            None
        } else if report.progressed == 0 {
            *interval = interval.saturating_mul(2).min(self.max);
            Some(*interval)
        } else {
            *interval = self.retry;
            Some(*interval)
        }
    }
}

/// Re-arms every projector after an inline projection left a retryable row.
/// The epoch is bumped first, so a re-arm that lands while a pass runs is
/// seen before the projector waits again.
static REARM: tokio::sync::Notify = tokio::sync::Notify::const_new();
static REARM_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// One legacy-file warning per process.
static LEGACY_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What one pass saw and changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PassReport {
    /// Authenticated retryable rows the pass saw.
    pub(crate) retryable_seen: usize,
    /// Rows whose state the pass changed.
    pub(crate) progressed: usize,
    /// Metered rows the pass left retryable (`pending` or `error`).
    pub(crate) retryable_left: usize,
    /// The scan reached the end of the retryable set. A failed, cancelled or
    /// deadline-cut pass is never exhausted.
    pub(crate) exhausted: bool,
}

/// The process-scoped projector task, owned by the process entry that
/// started it. It is cancelled with the process shutdown token (or by
/// [`Self::stop`] or drop); a cancelled pass stops at its next row and leaves
/// the unfinished rows retryable.
pub(crate) struct CostOutboxProjector {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for CostOutboxProjector {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl CostOutboxProjector {
    /// Start the projector eagerly, before the first turn, under the process
    /// `shutdown` token. Must be called inside a Tokio runtime.
    pub(crate) fn start(
        memory: Arc<dyn Memory>,
        workspace_dir: PathBuf,
        cost_config: CostConfig,
        shutdown: &CancellationToken,
    ) -> Self {
        Self::spawn(
            memory,
            workspace_dir,
            cost_config,
            shutdown.child_token(),
            Cadence::PROCESS,
        )
    }

    fn spawn(
        memory: Arc<dyn Memory>,
        workspace_dir: PathBuf,
        cost_config: CostConfig,
        cancel: CancellationToken,
        cadence: Cadence,
    ) -> Self {
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let mut interval = cadence.retry;
            loop {
                let epoch = REARM_EPOCH.load(std::sync::atomic::Ordering::Acquire);
                let report = pass_off_runtime(
                    Arc::clone(&memory),
                    workspace_dir.clone(),
                    cost_config.clone(),
                    task_cancel.clone(),
                )
                .await;
                if task_cancel.is_cancelled() {
                    return;
                }
                let wait = cadence.next_wait(&report, &mut interval);
                let notified = REARM.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if REARM_EPOCH.load(std::sync::atomic::Ordering::Acquire) != epoch {
                    // An inline failure re-armed during the pass.
                    continue;
                }
                match wait {
                    // Nothing retryable: wait for an inline failure to re-arm.
                    None => tokio::select! {
                        () = task_cancel.cancelled() => return,
                        () = &mut notified => {}
                    },
                    Some(delay) => tokio::select! {
                        () = task_cancel.cancelled() => return,
                        () = tokio::time::sleep(delay) => {}
                        () = &mut notified => {}
                    },
                }
            }
        });
        Self { cancel, task }
    }

    /// Cancel the task and wait for it to finish, at most one pass deadline
    /// (a pass in flight stops at its next row). Returns whether it finished.
    pub(crate) async fn stop(mut self) -> bool {
        self.cancel.cancel();
        let finished = tokio::time::timeout(PASS_DEADLINE, &mut self.task).await.is_ok();
        if !finished {
            tracing::warn!("recovery cost projector did not stop within the pass deadline");
        }
        finished
    }

    /// One pass with the 15 s deadline, for the one-shot `prx agent` entry
    /// before its turn.
    pub(crate) async fn run_once(memory: Arc<dyn Memory>, workspace_dir: PathBuf, cost_config: CostConfig) {
        pass_off_runtime(memory, workspace_dir, cost_config, CancellationToken::new()).await;
    }
}

/// Signal every projector that an inline projection left a retryable row.
pub(crate) fn rearm() {
    REARM_EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    REARM.notify_waiters();
}

/// One pass on the blocking pool. A failed pass reports no exhaustion, so
/// the scheduler retries it.
async fn pass_off_runtime(
    memory: Arc<dyn Memory>,
    workspace_dir: PathBuf,
    cost_config: CostConfig,
    cancel: CancellationToken,
) -> PassReport {
    let workspace_id = workspace_dir.to_string_lossy().into_owned();
    match crate::runtime::blocking::spawn_blocking(move || {
        run_pass(memory.as_ref(), &workspace_id, &workspace_dir, &cost_config, &cancel)
    })
    .await
    {
        Ok(Ok(report)) => report,
        Ok(Err(error)) => {
            tracing::warn!(%error, "recovery cost projection pass failed");
            PassReport::default()
        }
        Err(error) => {
            tracing::warn!(%error, "recovery cost projection worker failed");
            PassReport::default()
        }
    }
}

/// One projection pass over every retryable row, keyset-paged by settlement
/// id. It advances past rejected rows and stops only when the raw scan is
/// exhausted, the pass deadline passes or `cancel` fires.
pub(crate) fn run_pass(
    memory: &dyn Memory,
    workspace_id: &str,
    workspace_dir: &Path,
    cost_config: &CostConfig,
    cancel: &CancellationToken,
) -> anyhow::Result<PassReport> {
    let deadline = Instant::now() + PASS_DEADLINE;
    let stopped = || cancel.is_cancelled() || Instant::now() >= deadline;
    let mut report = PassReport::default();
    let Some(repository) = memory.recovery_repository() else {
        report.exhausted = true;
        return Ok(report);
    };
    if !repository.recovery_outbox_present()? {
        report.exhausted = true;
        return Ok(report);
    }
    let mut tracker: Option<Result<Arc<crate::cost::tracker::CostTracker>, RecoveryCostState>> = None;
    let mut after: Option<String> = None;
    loop {
        if stopped() {
            return Ok(report);
        }
        let scan = repository.scan_recovery_costs(workspace_id, after.as_deref(), PAGE)?;
        report.retryable_seen = report.retryable_seen.saturating_add(scan.rows.len());
        for row in &scan.rows {
            if stopped() {
                return Ok(report);
            }
            if row.pricing != RecoveryPricing::Metered {
                continue;
            }
            let opened = tracker.get_or_insert_with(|| open_tracker(cost_config, workspace_dir));
            let projected = project_row(repository, opened, workspace_id, row);
            if projected.progressed {
                report.progressed = report.progressed.saturating_add(1);
            }
            if is_retryable(projected.state) {
                report.retryable_left = report.retryable_left.saturating_add(1);
            }
        }
        if scan.exhausted {
            report.exhausted = true;
            return Ok(report);
        }
        after = scan.cursor;
    }
}

/// A row state the projector retries.
const fn is_retryable(state: RecoveryCostState) -> bool {
    matches!(state, RecoveryCostState::Pending | RecoveryCostState::Error)
}

fn open_tracker(
    cost_config: &CostConfig,
    workspace_dir: &Path,
) -> Result<Arc<crate::cost::tracker::CostTracker>, RecoveryCostState> {
    crate::cost::tracker::CostTracker::for_workspace(cost_config.clone(), workspace_dir).map_err(|error| {
        tracing::warn!(%error, "recovery cost projection could not open the ledger");
        if is_blocked(&error) {
            RecoveryCostState::Blocked
        } else {
            RecoveryCostState::Error
        }
    })
}

/// The state a row is left in by one projection, and whether it changed.
struct RowProjection {
    state: RecoveryCostState,
    progressed: bool,
}

/// Project one metered row and record its state.
fn project_row(
    repository: &dyn RecoveryRepository,
    tracker: &Result<Arc<crate::cost::tracker::CostTracker>, RecoveryCostState>,
    workspace_id: &str,
    row: &RecoveryCostRow,
) -> RowProjection {
    let unchanged = RowProjection {
        state: row.state,
        progressed: false,
    };
    let state = match tracker {
        Err(state) => *state,
        Ok(tracker) => {
            let usage = match usage_from_facts(&row.facts) {
                Ok(Some(usage)) => usage,
                Ok(None) => return unchanged,
                Err(error) => {
                    tracing::warn!(%error, settlement_id = %row.settlement_id, "recovery cost row facts are unreadable");
                    return unchanged;
                }
            };
            let projected = tracker.project_recovery_usage(&usage);
            #[cfg(test)]
            if projected
                .as_ref()
                .is_err_and(|error| error.is::<crate::cost::tracker::TestCrash>())
            {
                return unchanged;
            }
            match projected {
                Ok(()) => RecoveryCostState::Acknowledged,
                Err(error) if is_blocked(&error) => RecoveryCostState::Blocked,
                Err(_) => RecoveryCostState::Error,
            }
        }
    };
    if state == row.state {
        return unchanged;
    }
    match repository.finish_recovery_cost(workspace_id, &row.settlement_id, &row.usage_digest, state) {
        Ok(()) => RowProjection {
            state,
            progressed: true,
        },
        Err(error) => {
            tracing::warn!(%error, settlement_id = %row.settlement_id, "recovery cost finish failed");
            unchanged
        }
    }
}

/// The ledger refused the row permanently.
pub(crate) fn is_blocked(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<crate::cost::tracker::LedgerBlocked>())
}

/// The metered usage carried by outbox facts, if any.
pub(crate) fn usage_from_facts(facts: &[u8]) -> anyhow::Result<Option<MeteredTokenUsageRecord>> {
    let value: serde_json::Value = serde_json::from_slice(facts)?;
    match value.get("usage") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(usage) => Ok(Some(serde_json::from_value(usage.clone())?)),
    }
}

/// Pre-Part-2 decision files, relative to the workspace. They are never
/// opened: their presence is only reported.
const LEGACY_FILES: [&str; 3] = [
    "self/causal_tree/decisions.db",
    "self/causal_tree/.decision_key",
    "self/child_authority/journal.db",
];

/// Log one warning per process when a legacy decision file is present.
/// Uses `symlink_metadata` only: nothing is opened and `brain.db` is never
/// queried.
pub(crate) fn warn_legacy_files_once(workspace_dir: &Path) {
    warn_legacy_files(&LEGACY_WARNED, workspace_dir);
}

/// [`warn_legacy_files_once`] against an explicit once-flag; returns whether
/// this call logged the warning.
fn warn_legacy_files(warned: &std::sync::atomic::AtomicBool, workspace_dir: &Path) -> bool {
    if warned.load(std::sync::atomic::Ordering::Acquire) || !legacy_files_present(workspace_dir) {
        return false;
    }
    if warned.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return false;
    }
    tracing::warn!(
        "legacy decision files present under {}/self (ignored; start a fresh workspace to remove them)",
        workspace_dir.display()
    );
    true
}

fn legacy_files_present(workspace_dir: &Path) -> bool {
    LEGACY_FILES
        .iter()
        .any(|relative| std::fs::symlink_metadata(workspace_dir.join(relative)).is_ok())
}

#[cfg(test)]
#[path = "cost_recovery/tests.rs"]
mod tests;
