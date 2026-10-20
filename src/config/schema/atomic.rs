#[cfg(unix)]
use super::PermissionsExt;
use super::{AsyncWriteExt, Context, OpenOptions, Path, Result, fs};
#[cfg(unix)]
use super::{File, Permissions};

#[cfg(test)]
pub(crate) async fn write_toml_string_atomic(path: &Path, toml_str: &str) -> Result<()> {
    // FIX-P2-10: acquire a cross-process exclusive advisory lock before the
    // write+rename critical section. Without it, two PRX processes saving the
    // config concurrently can interleave their temp-write/backup/rename steps
    // and silently drop one process's update (last-writer-wins data loss). The
    // lock is created beside the config path (`<name>.<ext>.lock`) and is held
    // until this function returns (the guard is dropped after the rename and
    // permission fix-up complete). Reuses the existing, audited lock helper so
    // no new dependency or `unsafe` is introduced.
    let _config_write_lock = crate::self_system::evolution::safety_utils::acquire_file_lock(path).await?;

    write_toml_string_atomic_without_lock(path, toml_str).await
}

/// Replace one TOML file atomically while the caller owns the enclosing
/// configuration transaction lock.
///
/// This is crate-visible only so multi-file config commits can hold one lock
/// across the complete generation instead of deadlocking on per-file locks.
pub(crate) async fn write_toml_string_atomic_without_lock(path: &Path, toml_str: &str) -> Result<()> {
    let parent_dir = path.parent().context("Config path must have a parent directory")?;

    if let Ok(metadata) = fs::symlink_metadata(path).await {
        if metadata.file_type().is_symlink() {
            anyhow::bail!("Refusing to replace config via symlink path: {}", path.display());
        }
    }

    fs::create_dir_all(parent_dir)
        .await
        .with_context(|| format!("Failed to create config directory: {}", parent_dir.display()))?;

    let file_name = path.file_name().and_then(|v| v.to_str()).unwrap_or("config.toml");
    let temp_path = parent_dir.join(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));
    let backup_path = parent_dir.join(format!("{file_name}.bak"));

    let mut open_options = OpenOptions::new();
    open_options.create_new(true).write(true);
    #[cfg(unix)]
    open_options.mode(0o600);

    let mut temp_file = open_options
        .open(&temp_path)
        .await
        .with_context(|| format!("Failed to create temporary config file: {}", temp_path.display()))?;
    temp_file
        .write_all(toml_str.as_bytes())
        .await
        .context("Failed to write temporary config contents")?;
    temp_file
        .sync_all()
        .await
        .context("Failed to fsync temporary config file")?;
    drop(temp_file);

    let had_existing_config = path.exists();
    if had_existing_config {
        fs::copy(path, &backup_path).await.with_context(|| {
            format!(
                "Failed to create config backup before atomic replace: {}",
                backup_path.display()
            )
        })?;
    }

    if let Err(e) = fs::rename(&temp_path, path).await {
        let _ = fs::remove_file(&temp_path).await;
        if had_existing_config && backup_path.exists() {
            fs::copy(&backup_path, path)
                .await
                .context("Failed to restore config backup")?;
        }
        anyhow::bail!("Failed to atomically replace config file: {e}");
    }

    sync_directory(parent_dir).await?;

    #[cfg(unix)]
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .await
        .with_context(|| format!("Failed to restrict permissions on {}", path.display()))?;

    if had_existing_config {
        let _ = fs::remove_file(&backup_path).await;
    }

    Ok(())
}

pub(crate) async fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = File::open(path)
            .await
            .with_context(|| format!("Failed to open directory for fsync: {}", path.display()))?;
        dir.sync_all()
            .await
            .with_context(|| format!("Failed to fsync directory metadata: {}", path.display()))?;
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
