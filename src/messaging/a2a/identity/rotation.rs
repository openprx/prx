//! Serialize activation and remove key generations no longer used locally.
use super::{Record, read};
use anyhow::{Result, ensure};
use std::{
    fs::{self, OpenOptions},
    path::Path,
};
pub(super) fn lock(dir: &Path) -> Result<fs::File> {
    let path = dir.join("identity.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&path)?;
    ensure!(file.metadata()?.is_file(), "invalid identity lease");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            file.metadata()?.permissions().mode().trailing_zeros() >= 7,
            "identity lease permissions exceed 0600"
        );
    }
    file.lock()?;
    Ok(file)
}
pub(super) fn cleanup(dir: &Path) -> Result<()> {
    let mut keep = std::collections::HashSet::new();
    for name in ["identity.toml", "pending.toml"] {
        let path = dir.join(name);
        if path.try_exists()? {
            let record: Record = toml::from_str(std::str::from_utf8(&read(&path, true)?)?)?;
            keep.insert(record.generation);
        }
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let key = name.strip_suffix(".key.pem").or_else(|| name.strip_suffix(".der"));
        let generation = key.and_then(|name| uuid::Uuid::parse_str(name).ok());
        let temporary = name
            .strip_prefix('.')
            .is_some_and(|name| uuid::Uuid::parse_str(name).is_ok());
        if temporary || generation.is_some_and(|id| !keep.contains(&id)) {
            fs::remove_file(entry.path())?;
        }
    }
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
