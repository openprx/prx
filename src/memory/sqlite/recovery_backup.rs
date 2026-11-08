//! Coherent database backup that carries every recovery key its snapshot
//! references. Key bytes move from the source key directory straight into the
//! staged key directory; they are never returned, kept or logged.

use super::recovery_keys::{INCOMPLETE_MARKER, INCOMPLETE_PREFIX, KeyDirectory, ensure_admitted};
use anyhow::Context;
use rusqlite::{Connection, OpenFlags, params};
use std::collections::BTreeSet;
use std::path::Path;

/// Points at which a test can fail or interrupt an attempt, after the side
/// effects of the preceding step.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Before each key is installed into the staged key directory.
    Install,
    /// After each key is installed.
    AfterInstall,
    /// Everything is staged and durable; nothing is published.
    Staged,
    /// The directory is renamed into place; the incomplete marker is still there.
    Renamed,
    /// The marker is gone and the directory is durable.
    Published,
    /// A failed bundle was renamed to its quarantine name; its parent is next.
    Barrier,
    /// The quarantine rename is durable; deleting its contents is next.
    Cleanup,
}

#[cfg(test)]
type Hook = Box<dyn Fn(Step, &Path) -> anyhow::Result<()>>;

#[cfg(test)]
thread_local! {
    static FAULT: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn fault(step: Step, at: &Path) -> anyhow::Result<()> {
    FAULT.with(|slot| slot.borrow().as_ref().map_or(Ok(()), |hook| hook(step, at)))
}

/// Run `run` on this thread with `hook` observing every step.
#[cfg(test)]
pub(super) fn with_fault<R>(hook: impl Fn(Step, &Path) -> anyhow::Result<()> + 'static, run: impl FnOnce() -> R) -> R {
    FAULT.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    let out = run();
    FAULT.with(|slot| *slot.borrow_mut() = None);
    out
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

/// Write a snapshot of `conn` as `target`, inside a new owner-only bundle
/// directory (`target`'s parent) that also holds `.recovery_keys` with every
/// key generation the snapshot references. The whole bundle is staged in a
/// private sibling directory and published by one atomic rename; the incomplete
/// marker is removed only after that rename, so every state before it is
/// refused as a key repository and cannot start a fresh authority. On any error
/// only this attempt's files are removed. The bundle directory must not exist
/// or must be empty.
pub(super) fn backup_bundle(conn: &Connection, target: &Path) -> anyhow::Result<()> {
    let target = std::path::absolute(target)?;
    let bundle = target.parent().context("backup target has no parent directory")?;
    let file_name = target.file_name().context("backup target has no file name")?;
    let root = bundle.parent().context("backup directory has no parent directory")?;
    // Reserved names and incomplete attempts are refused before any side effect.
    ensure_admitted(bundle)?;
    std::fs::create_dir_all(root)?;
    let vacant = match std::fs::read_dir(bundle) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(vacant, "backup directory is not empty: {}", bundle.display());
    // The attempt-unique name alone makes the directory unrestorable, so the
    // incomplete state holds from creation on; it is made durable before any
    // snapshot work, and the marker keeps it so after the rename.
    let staging = root.join(format!("{INCOMPLETE_PREFIX}{}", uuid::Uuid::new_v4()));
    create_private_dir(&staging)?;
    // The marker follows the mkdir at once; then the directory and its parent
    // are made durable, all before any snapshot work.
    let marked = std::fs::File::create_new(staging.join(INCOMPLETE_MARKER))
        .and_then(|marker| marker.sync_all())
        .map_err(anyhow::Error::from)
        .and_then(|()| sync_directory(&staging))
        .and_then(|()| sync_directory(root));
    let outcome = marked.and_then(|()| stage_and_publish(conn, file_name, bundle, root, &staging));
    // After a successful rename the staging path is gone; this only cleans failures.
    discard(std::fs::remove_dir_all(&staging));
    outcome
}

fn stage_and_publish(
    conn: &Connection,
    file_name: &std::ffi::OsStr,
    bundle: &Path,
    root: &Path,
    staging: &Path,
) -> anyhow::Result<()> {
    let staged = staging.join(file_name);
    // A coherent snapshot: a live write-ahead log is never copied byte for byte.
    conn.execute("VACUUM INTO ?1", params![staged.to_string_lossy().to_string()])?;
    std::fs::File::open(&staged)?.sync_all()?;
    let ids = {
        let snapshot = Connection::open_with_flags(&staged, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        referenced_key_ids(&snapshot)?
    };
    if !ids.is_empty() {
        let memory = conn
            .path()
            .filter(|path| !path.is_empty())
            .context("an in-memory store has no recovery keys")?;
        let source = KeyDirectory::open(Path::new(memory).parent().context("missing memory directory")?, false)?;
        let installed = KeyDirectory::open_staging(staging)?;
        for id in &ids {
            // A referenced key that cannot be read refuses the whole backup.
            let bytes = source.read(id)?;
            #[cfg(test)]
            fault(Step::Install, staging)?;
            installed.install(id, &bytes)?;
            #[cfg(test)]
            fault(Step::AfterInstall, staging)?;
        }
    }
    sync_directory(staging)?;
    #[cfg(test)]
    fault(Step::Staged, staging)?;
    // Keys and database become visible together, or not at all.
    std::fs::rename(staging, bundle)?;
    // The bundle location is this attempt's from here on: a failure removes it.
    let finished = finish(bundle, root);
    if finished.is_err() {
        quarantine(bundle, root);
    }
    finished
}

fn finish(bundle: &Path, root: &Path) -> anyhow::Result<()> {
    sync_directory(root)?;
    #[cfg(test)]
    fault(Step::Renamed, bundle)?;
    std::fs::remove_file(bundle.join(INCOMPLETE_MARKER))?;
    sync_directory(bundle)?;
    sync_directory(root)?;
    #[cfg(test)]
    fault(Step::Published, bundle)?;
    Ok(())
}

/// Remove a failed published bundle. It is first renamed to a quarantine name
/// that is refused as a key repository, and that rename is made durable before
/// any content is deleted; if the barrier fails the intact, refused directory
/// is kept. An interrupted delete therefore never leaves a restorable remainder.
fn quarantine(bundle: &Path, root: &Path) {
    let quarantined = root.join(format!("{INCOMPLETE_PREFIX}q-{}", uuid::Uuid::new_v4()));
    if let Err(error) = std::fs::rename(bundle, &quarantined) {
        tracing::warn!(%error, "failed recovery backup was not quarantined");
        // Make the marker durable (file, then directory) before relying on it.
        let marked = std::fs::File::create(bundle.join(INCOMPLETE_MARKER))
            .and_then(|marker| marker.sync_all())
            .map_err(anyhow::Error::from)
            .and_then(|()| sync_directory(bundle));
        if let Err(error) = marked {
            tracing::warn!(%error, "failed recovery backup is not marked incomplete");
        }
        return;
    }
    let barrier = sync_directory(root).and_then(|()| {
        #[cfg(test)]
        fault(Step::Barrier, &quarantined)?;
        Ok(())
    });
    if let Err(error) = barrier {
        tracing::warn!(%error, "quarantine of a failed recovery backup is not durable");
        return;
    }
    #[cfg(test)]
    if fault(Step::Cleanup, &quarantined).is_err() {
        return;
    }
    discard(std::fs::remove_dir_all(&quarantined));
}

fn discard(removed: std::io::Result<()>) {
    if let Err(error) = removed {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(%error, "unpublished recovery backup was not removed");
        }
    }
}
