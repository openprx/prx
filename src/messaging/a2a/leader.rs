//! A process lease prevents multiple local chat/daemon outbox dispatchers.
use anyhow::{Result, ensure};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};
pub(super) fn open(workspace: &Path) -> Result<File> {
    let path = workspace.join("a2a").join("outbox.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&path)?;
    ensure!(
        file.metadata()?.is_file() && !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
        "invalid outbox lease file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            file.metadata()?.permissions().mode().trailing_zeros() >= 7,
            "outbox lease permissions exceed 0600"
        );
    }
    Ok(file)
}
pub(super) fn claim(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}
