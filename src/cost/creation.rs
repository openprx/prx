//! Failed creation durability must not turn a retry into an existing-file append.
#[cfg(unix)]
pub(super) fn sync_created_entry(
    file: &std::fs::File,
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    directory: &std::fs::File,
) -> anyhow::Result<()> {
    sync_created_entry_with(file, parent, name, || directory.sync_all())
}

#[cfg(unix)]
fn sync_created_entry_with(
    file: &std::fs::File,
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    sync: impl FnOnce() -> std::io::Result<()>,
) -> anyhow::Result<()> {
    use anyhow::Context;
    if let Err(error) = sync() {
        // Never unlink a replacement installed at the same name. Callers hold
        // the workspace ledger lock and pass only files they just created.
        let opened = rustix::fs::fstat(file)?;
        let named = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
        anyhow::ensure!(
            opened.st_dev == named.st_dev && opened.st_ino == named.st_ino,
            "new cost entry changed before creation rollback"
        );
        rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty())
            .context("failed to roll back unsynced new cost entry")?;
        return Err(error).context("failed to sync new cost entry directory");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};

    #[test]
    fn failed_directory_sync_removes_only_the_new_entry_and_retry_syncs() {
        let workspace = tempfile::tempdir().unwrap();
        let parent = File::open(workspace.path()).unwrap();
        let path = workspace.path().join("costs.jsonl");
        let name = std::ffi::OsStr::new("costs.jsonl");
        let file = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        assert!(
            sync_created_entry_with(&file, &parent, name, || {
                Err(std::io::Error::other("injected directory sync failure"))
            })
            .is_err()
        );
        assert!(!path.exists());
        let retry = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        sync_created_entry(&retry, &parent, name, &parent).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn failed_directory_sync_never_unlinks_a_replacement() {
        let workspace = tempfile::tempdir().unwrap();
        let parent = File::open(workspace.path()).unwrap();
        let path = workspace.path().join("costs.jsonl");
        let file = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        std::fs::rename(&path, workspace.path().join("original.jsonl")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert!(
            sync_created_entry_with(&file, &parent, std::ffi::OsStr::new("costs.jsonl"), || {
                Err(std::io::Error::other("injected directory sync failure"))
            })
            .is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
    }
}
