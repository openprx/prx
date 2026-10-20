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
    with_created_entry(file, parent, name, || {
        use anyhow::Context;
        sync().context("failed to sync new cost entry directory")
    })
}

/// Cover every fallible operation after create, including body write and sync.
#[cfg(unix)]
pub(super) fn with_created_entry<T>(
    file: &std::fs::File,
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    use anyhow::Context;
    match operation() {
        Ok(result) => Ok(result),
        Err(error) => {
            let opened = rustix::fs::fstat(file)?;
            let named = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
            anyhow::ensure!(
                opened.st_dev == named.st_dev && opened.st_ino == named.st_ino,
                "new cost entry changed before creation rollback"
            );
            rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty())
                .context("failed to roll back unsynced new cost entry")?;
            Err(error)
        }
    }
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
    #[test]
    fn failed_operation_after_body_write_rolls_back_created_file_before_retry() {
        use std::io::Write;
        let workspace = tempfile::tempdir().unwrap();
        let parent = File::open(workspace.path()).unwrap();
        let path = workspace.path().join("costs.jsonl");
        let file = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        let result = with_created_entry(&file, &parent, std::ffi::OsStr::new("costs.jsonl"), || {
            (&file).write_all(b"partial body")?;
            Err::<(), _>(std::io::Error::other("injected append failure").into())
        });
        assert!(result.is_err());
        assert!(!path.exists());
        assert!(OpenOptions::new().write(true).create_new(true).open(path).is_ok());
    }
}
