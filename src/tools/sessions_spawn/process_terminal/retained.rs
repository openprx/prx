//! Process-worker snapshot folders (`workers/<run_id>/.prx-worker-*`) that
//! outlived their owner, and the start-up sweep that settles them.
//!
//! An owner removes the folder right after it reaps its worker. A folder whose
//! signed launch shows the worker process absent in this boot context
//! therefore has no live owner: the owner died before it could reap and
//! clean up. Such a folder gets one durable terminal row
//! ([`OwnerLossReason::OwnerGone`], keyed by run) and is removed. A folder
//! whose worker is still present, or whose launch cannot be verified, is left
//! alone. Age is never used.
use crate::memory::{MemoryFabric, MemoryVisibility, MessageEvent, MessageEventScope};
use crate::tools::sessions_spawn::OwnerLossReason;
use std::path::{Path, PathBuf};

/// Snapshot directory prefix written by `PreparedManifest::prepare`.
const SNAPSHOT_PREFIX: &str = ".prx-worker-";
/// Run directories looked at per scan.
const MAX_SCANNED_RUN_DIRECTORIES: usize = 4096;
/// Entries looked at inside one run directory.
const MAX_ENTRIES_PER_RUN_DIRECTORY: usize = 64;
/// Folders settled per process start; the rest wait for the next start.
const MAX_SWEPT_FOLDERS: usize = 64;
/// One retained snapshot folder.
pub(crate) struct RetainedFolder {
    pub(crate) run_id: String,
    pub(crate) path: PathBuf,
}

/// A bounded listing of retained snapshot folders.
#[derive(Default)]
pub(crate) struct RetainedScan {
    pub(crate) folders: Vec<RetainedFolder>,
    /// More run directories exist than one scan looks at.
    pub(crate) truncated: bool,
}

/// List retained snapshot folders under `worker_root`. Read-only; symbolic
/// links are never followed. A missing root is an empty listing.
pub(crate) fn scan_retained_folders(worker_root: &Path) -> std::io::Result<RetainedScan> {
    let mut scan = RetainedScan::default();
    let entries = match std::fs::read_dir(worker_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(scan),
        Err(error) => return Err(error),
    };
    for (index, entry) in entries.enumerate() {
        if index == MAX_SCANNED_RUN_DIRECTORIES {
            scan.truncated = true;
            break;
        }
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(run_id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let run_entries = match std::fs::read_dir(entry.path()) {
            Ok(run_entries) => run_entries,
            // Removed concurrently by its owner.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for run_entry in run_entries.take(MAX_ENTRIES_PER_RUN_DIRECTORY) {
            let run_entry = run_entry?;
            let is_snapshot = run_entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(SNAPSHOT_PREFIX));
            if is_snapshot && run_entry.file_type()?.is_dir() {
                scan.folders.push(RetainedFolder {
                    run_id: run_id.clone(),
                    path: run_entry.path(),
                });
            }
        }
    }
    Ok(scan)
}

/// What a signed launch says about a retained folder's worker.
pub(super) struct RetainedLaunch {
    /// The launched worker process is absent in this boot context.
    pub(super) worker_absent: bool,
    /// Scope of the run's terminal row.
    pub(super) scope: MessageEventScope,
}

/// The terminal row's scope, taken from the run's authenticated launch rows.
pub(super) fn terminal_scope(event: &MessageEvent) -> MessageEventScope {
    let mut scope = MessageEventScope::new("sessions_spawn", MemoryVisibility::Workspace);
    scope.owner_id.clone_from(&event.owner_id);
    scope.run_id.clone_from(&event.run_id);
    scope.session_key.clone_from(&event.session_key);
    scope.parent_session_key.clone_from(&event.parent_session_key);
    scope.parent_run_id.clone_from(&event.parent_run_id);
    scope.channel.clone_from(&event.channel);
    scope.agent_id.clone_from(&event.agent_id);
    scope.persona_id.clone_from(&event.persona_id);
    scope.config_generation_id = event.config_generation_id;
    scope.config_source_revision.clone_from(&event.config_source_revision);
    scope
}

/// The signed launch of `run`, ordinary or operator, if one was recorded.
async fn launch_of(fabric: &MemoryFabric, workspace: &Path, run: &str) -> anyhow::Result<Option<RetainedLaunch>> {
    let memory = fabric.memory();
    let ordinary = format!("worker:{run}:ordinary-launch");
    if let Some(candidate) = memory
        .find_message_event_by_idempotency_key(fabric.workspace_id(), &ordinary)
        .await?
    {
        // The candidate only names the owner to look under; the launch is
        // authenticated by its seal before any fact is used.
        let owner = candidate.owner_id.unwrap_or_default();
        return super::ordinary::retained_launch(fabric, workspace, &owner, run).await;
    }
    let operator = format!("worker:{run}:operator-launch");
    if let Some(candidate) = memory
        .find_message_event_by_idempotency_key(fabric.workspace_id(), &operator)
        .await?
    {
        let owner = candidate.owner_id.unwrap_or_default();
        return super::operator::retained_launch(fabric, workspace, &owner, run).await;
    }
    Ok(None)
}

/// Counts of one start-up sweep.
#[derive(Debug, Default, PartialEq, Eq)]
struct SweepSummary {
    retained: usize,
    settled: usize,
    owner_alive: usize,
    unverified: usize,
    deferred: usize,
}

/// Settle retained snapshot folders whose owner is gone. Runs once per
/// process start and handles at most [`MAX_SWEPT_FOLDERS`] folders. Writing
/// the row before removing the folder keeps it idempotent: a crash in between
/// rewrites the same keyed row on the next start, which the store ignores.
pub(super) async fn sweep_retained_worker_folders(
    fabric: &MemoryFabric,
    workspace: &Path,
    worker_root: &Path,
    runs: &tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>,
) {
    let scan = match scan_retained_folders(worker_root) {
        Ok(scan) => scan,
        Err(error) => {
            tracing::warn!(
                error = %crate::agent::diagnostics::error_class(&error),
                "retained worker folder scan unavailable"
            );
            return;
        }
    };
    let mut summary = SweepSummary {
        retained: scan.folders.len(),
        deferred: scan.folders.len().saturating_sub(MAX_SWEPT_FOLDERS),
        ..SweepSummary::default()
    };
    for folder in scan.folders.into_iter().take(MAX_SWEPT_FOLDERS) {
        let owned_here = runs.read().await.iter().any(|run| {
            run.id == folder.run_id
                && run
                    .process_control
                    .as_ref()
                    .is_some_and(|control| !control.recovery_only)
        });
        if owned_here {
            summary.owner_alive += 1;
            continue;
        }
        let launch = match launch_of(fabric, workspace, &folder.run_id).await {
            Ok(Some(launch)) => launch,
            Ok(None) => {
                summary.unverified += 1;
                continue;
            }
            Err(error) => {
                tracing::debug!(
                    error = %crate::agent::diagnostics::error_class(&error),
                    "retained worker launch unverified"
                );
                summary.unverified += 1;
                continue;
            }
        };
        if !launch.worker_absent {
            summary.owner_alive += 1;
            continue;
        }
        if let Err(error) =
            super::super::record_owner_loss_terminal(fabric, launch.scope, &folder.run_id, OwnerLossReason::OwnerGone)
                .await
        {
            tracing::warn!(
                error = %crate::agent::diagnostics::error_class(&error),
                "owner-gone terminal row remains unconfirmed; folder kept"
            );
            summary.unverified += 1;
            continue;
        }
        match remove_snapshot_folder(&folder) {
            Ok(()) => summary.settled += 1,
            Err(error) => {
                tracing::warn!(
                    error = %crate::agent::diagnostics::error_class(&error),
                    "retained worker folder removal failed"
                );
                summary.unverified += 1;
            }
        }
    }
    tracing::info!(
        retained = summary.retained,
        settled = summary.settled,
        owner_alive = summary.owner_alive,
        unverified = summary.unverified,
        deferred = summary.deferred,
        truncated = scan.truncated,
        "retained worker folder sweep finished"
    );
}

/// Remove one snapshot folder (a real directory, never a link), then its run
/// directory only if that is now empty.
fn remove_snapshot_folder(folder: &RetainedFolder) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(&folder.path)?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "retained worker snapshot is not a directory",
        ));
    }
    std::fs::remove_dir_all(&folder.path)?;
    if let Some(run_directory) = folder.path.parent() {
        // Succeeds only when nothing else of the worker's workspace remains.
        if std::fs::remove_dir(run_directory).is_err() {
            tracing::debug!("retained worker run directory kept: not empty");
        }
    }
    Ok(())
}
