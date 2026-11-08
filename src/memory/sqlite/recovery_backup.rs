//! Coherent database backup that carries every recovery key its snapshot
//! references. Key bytes move from the source key directory straight into the
//! staged key directory; they are never returned, kept or logged.

use super::recovery_keys::{INCOMPLETE_MARKER, KEYS_DIRECTORY, KeyDirectory};
use anyhow::Context;
use rusqlite::{Connection, OpenFlags, params};
use std::collections::BTreeSet;
use std::path::Path;

/// Points at which an attempt can fail or be interrupted. Production passes a
/// hook that always succeeds; tests inject failures and simulated crashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Before each key is installed into the staged key directory.
    Install,
    /// After everything is staged and durable, before anything is published.
    BeforePublish,
}

/// Every persisted column that names a recovery key generation. A later
/// table that stores a key id joins this list.
const REFERENCES: [(&str, &str); 5] = [
    ("recovery_authority", "SELECT active_key_id FROM recovery_authority"),
    ("recovery_attestations", "SELECT key_id FROM recovery_attestations"),
    (
        "recovery_transport_index",
        "SELECT key_id FROM recovery_transport_index",
    ),
    ("recovery_tombstones", "SELECT key_id FROM recovery_tombstones"),
    (
        "ingress_recovery",
        "SELECT json_extract(capsule_json, '$.key_id') FROM ingress_recovery WHERE json_valid(capsule_json)",
    ),
];

/// Key ids referenced by a snapshot. A snapshot without the recovery tables
/// references none.
fn referenced_key_ids(snapshot: &Connection) -> anyhow::Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for (table, select) in REFERENCES {
        let present: bool = snapshot.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1 AND type='table')",
            [table],
            |r| r.get(0),
        )?;
        if !present {
            continue;
        }
        let mut statement = snapshot.prepare(select)?;
        for id in statement.query_map([], |r| r.get::<_, Option<String>>(0))? {
            ids.extend(id?);
        }
    }
    Ok(ids)
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir(path)?;
    Ok(())
}

fn sync_directory(path: &Path) -> anyhow::Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

/// Write a snapshot of `conn` to `target`, plus the `.recovery_keys` directory
/// next to it holding every key generation the snapshot references. Both are
/// staged in a private sibling directory and published only when complete: on
/// any error nothing is left at the target, and only this attempt's staging
/// directory is removed. A staging directory abandoned by a crash carries the
/// incomplete marker and is refused as a key repository.
pub(super) fn backup_bundle(
    conn: &Connection,
    target: &Path,
    fault: &dyn Fn(Step, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let target = std::path::absolute(target)?;
    let parent = target.parent().context("backup target has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    anyhow::ensure!(!target.exists(), "backup target already exists: {}", target.display());
    let staging = parent.join(format!(".backup-staging-{}", uuid::Uuid::new_v4()));
    create_private_dir(&staging)?;
    let outcome = stage_and_publish(conn, &target, parent, &staging, fault);
    discard(std::fs::remove_dir_all(&staging));
    outcome
}

fn stage_and_publish(
    conn: &Connection,
    target: &Path,
    parent: &Path,
    staging: &Path,
    fault: &dyn Fn(Step, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    std::fs::File::create_new(staging.join(INCOMPLETE_MARKER))?.sync_all()?;
    let staged = staging.join(target.file_name().context("backup target has no file name")?);
    // A coherent snapshot: a live write-ahead log is never copied byte for byte.
    conn.execute("VACUUM INTO ?1", params![staged.to_string_lossy().to_string()])?;
    std::fs::File::open(&staged)?.sync_all()?;
    let ids = {
        let snapshot = Connection::open_with_flags(&staged, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        referenced_key_ids(&snapshot)?
    };
    let keys_destination = parent.join(KEYS_DIRECTORY);
    if !ids.is_empty() {
        anyhow::ensure!(
            std::fs::symlink_metadata(&keys_destination).is_err(),
            "a recovery key directory already exists next to the backup target"
        );
        let memory = conn
            .path()
            .filter(|path| !path.is_empty())
            .context("an in-memory store has no recovery keys")?;
        let source = KeyDirectory::open(Path::new(memory).parent().context("missing memory directory")?, false)?;
        let installed = KeyDirectory::open_staging(staging)?;
        for id in &ids {
            // A referenced key that cannot be read refuses the whole backup.
            let bytes = source.read(id)?;
            fault(Step::Install, staging)?;
            installed.install(id, &bytes)?;
        }
    }
    sync_directory(staging)?;
    fault(Step::BeforePublish, staging)?;
    // Keys first: a database is never visible without the keys it needs.
    if !ids.is_empty() {
        std::fs::rename(staging.join(KEYS_DIRECTORY), &keys_destination)?;
        sync_directory(parent)?;
    }
    let published = std::fs::rename(&staged, target)
        .map_err(anyhow::Error::from)
        .and_then(|()| sync_directory(parent));
    if let Err(error) = published {
        // Both paths were absent before this attempt, so removing them is safe.
        discard(std::fs::remove_file(target));
        if !ids.is_empty() {
            discard(std::fs::remove_dir_all(&keys_destination));
        }
        return Err(error);
    }
    Ok(())
}

fn discard(removed: std::io::Result<()>) {
    if let Err(error) = removed {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(%error, "unpublished recovery backup was not removed");
        }
    }
}
