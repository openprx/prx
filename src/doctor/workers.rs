//! `prx doctor`: process-worker snapshot folders (`workers/<run>/.prx-worker-*`)
//! still on disk. Read-only: the store is opened read-only, a launch is
//! trusted only after its seal verifies, and a snapshot is checked by its
//! top-level key names only — no value is read into the report.
use super::{Config, DiagItem};

const CATEGORY: &str = "workers";

/// Launch idempotency keys of one run, ordinary first.
fn launch_keys(run: &str) -> [String; 2] {
    [
        format!("worker:{run}:ordinary-launch"),
        format!("worker:{run}:operator-launch"),
    ]
}

/// `Some(true)` when the run's signed launch shows its worker gone in this
/// boot context, `Some(false)` when it is still present, `None` when no
/// launch can be verified.
fn owner_gone(connection: &rusqlite::Connection, workspace: &str, run: &str) -> anyhow::Result<Option<bool>> {
    use rusqlite::OptionalExtension as _;
    for key in launch_keys(run) {
        let row: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT event_type, raw_payload_json FROM message_events
                 WHERE workspace_id = ?1 AND idempotency_key = ?2 LIMIT 1",
                rusqlite::params![workspace, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((event_type, payload)) = row {
            let payload = payload.ok_or_else(|| anyhow::anyhow!("worker launch payload missing"))?;
            return crate::tools::sessions_spawn::stored_launch_owner_gone(workspace, run, &event_type, &payload)
                .map(Some);
        }
    }
    Ok(None)
}

pub(super) fn check_retained_worker_folders(config: &Config, items: &mut Vec<DiagItem>) {
    let root = crate::tools::sessions_spawn::resolve_worker_workspace_root(
        config.sessions_spawn.worker_workspace_root.as_deref(),
        &config.workspace_dir,
    );
    let scan = match crate::tools::sessions_spawn::scan_retained_folders(&root) {
        Ok(scan) => scan,
        Err(error) => {
            items.push(DiagItem::warn(
                CATEGORY,
                format!("retained worker folders could not be listed ({})", error.kind()),
            ));
            return;
        }
    };
    if scan.folders.is_empty() {
        items.push(DiagItem::healthy(CATEGORY, "0 retained worker folders"));
        return;
    }
    let database = crate::schema_migration::memory_db_path(config);
    let connection = if database.is_file() {
        rusqlite::Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
    } else {
        None
    };
    let workspace = config.workspace_dir.to_string_lossy();
    let (mut dead, mut unverified, mut credential, mut unreadable) = (0_usize, 0_usize, 0_usize, 0_usize);
    for folder in &scan.folders {
        match connection
            .as_ref()
            .map(|connection| owner_gone(connection, &workspace, &folder.run_id))
        {
            Some(Ok(Some(true))) => dead += 1,
            Some(Ok(Some(false))) => {}
            Some(Ok(None) | Err(_)) | None => unverified += 1,
        }
        match crate::tools::sessions_spawn::snapshot_carries_credential_field(&folder.path) {
            Ok(true) => credential += 1,
            Ok(false) => {}
            Err(_) => unreadable += 1,
        }
    }
    let message = format!(
        "{} retained worker folder(s){}: {dead} with a dead owner, {unverified} unverified; \
         a credential field is present in {credential} manifest(s){}",
        scan.folders.len(),
        if scan.truncated { " (listing truncated)" } else { "" },
        if unreadable > 0 {
            format!(", {unreadable} manifest(s) unreadable")
        } else {
            String::new()
        },
    );
    if credential > 0 {
        items.push(DiagItem::error(CATEGORY, message));
    } else if dead > 0 || unverified > 0 || unreadable > 0 {
        // The next `prx chat`, gateway or channel start settles dead-owner folders.
        items.push(DiagItem::warn(CATEGORY, message));
    } else {
        items.push(DiagItem::ok(CATEGORY, message));
    }
}
