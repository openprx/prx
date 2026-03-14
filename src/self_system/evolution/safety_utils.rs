use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
use uuid::Uuid;

const RAW_LOG_DEBUG_ENV: &str = "OPENPRX_EVOLUTION_DEBUG_RAW";
const RAW_LOG_DEBUG_ENV_LEGACY: &str = "ZEROCLAW_EVOLUTION_DEBUG_RAW";

/// Resolve and validate a workspace-relative path, rejecting traversal and escape.
pub fn validate_path_in_workspace(workspace_root: &Path, target: &Path) -> Result<PathBuf> {
    if target.is_absolute() {
        bail!("absolute paths are not allowed: {}", target.display());
    }
    if target
        .components()
        .any(|item| matches!(item, Component::ParentDir))
    {
        bail!("parent traversal is not allowed: {}", target.display());
    }

    let canonical_root = workspace_root.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize workspace root: {}",
            workspace_root.display()
        )
    })?;

    let joined = canonical_root.join(target);
    let canonical_target = canonicalize_for_workspace_target(&joined)?;
    if !canonical_target.starts_with(&canonical_root) {
        bail!(
            "target path escapes workspace: {}",
            canonical_target.display()
        );
    }

    Ok(canonical_target)
}

fn canonicalize_for_workspace_target(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }

    let mut cursor = path;
    let mut missing = Vec::new();
    while cursor.canonicalize().is_err() {
        let name = cursor
            .file_name()
            .map(|v| v.to_os_string())
            .context("target path has no canonicalizable ancestor")?;
        missing.push(name);
        cursor = cursor
            .parent()
            .context("target path has no parent for canonicalization")?;
    }

    let mut resolved = cursor
        .canonicalize()
        .with_context(|| format!("failed to canonicalize ancestor: {}", cursor.display()))?;
    while let Some(segment) = missing.pop() {
        resolved.push(segment);
    }
    Ok(resolved)
}

fn normalize_target_in_workspace(
    workspace_root: &Path,
    target: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let canonical_root = workspace_root.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize workspace root: {}",
            workspace_root.display()
        )
    })?;
    let relative = if target.is_absolute() {
        target.strip_prefix(&canonical_root).with_context(|| {
            format!(
                "target path is outside workspace: target={}, workspace={}",
                target.display(),
                canonical_root.display()
            )
        })?
    } else {
        target
    };
    let canonical_target = validate_path_in_workspace(&canonical_root, relative)?;
    Ok((canonical_root, canonical_target))
}

async fn ensure_atomic_tmp_dir(canonical_root: &Path) -> Result<PathBuf> {
    let tmp_dir = validate_path_in_workspace(canonical_root, Path::new(".evolution/.atomic_tmp"))?;
    fs::create_dir_all(&tmp_dir).await?;
    let canonical_tmp = tmp_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize atomic tmp dir: {}",
            tmp_dir.display()
        )
    })?;
    if canonical_tmp != tmp_dir {
        bail!(
            "atomic tmp dir resolved through symlink: expected={}, actual={}",
            tmp_dir.display(),
            canonical_tmp.display()
        );
    }
    if !canonical_tmp.starts_with(canonical_root) {
        bail!(
            "atomic tmp dir escapes workspace root: tmp={}, workspace={}",
            canonical_tmp.display(),
            canonical_root.display()
        );
    }
    Ok(tmp_dir)
}

#[cfg(test)]
fn atomic_write_test_hook_cell() -> &'static std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>> {
    use std::sync::{Mutex, OnceLock};
    static HOOK: OnceLock<Mutex<Option<Box<dyn Fn() + Send + Sync>>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn run_atomic_write_test_hook() {
    if let Some(callback) = atomic_write_test_hook_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        callback();
    }
}

#[cfg(test)]
fn set_atomic_write_test_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *atomic_write_test_hook_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = hook;
}

#[cfg(not(test))]
fn run_atomic_write_test_hook() {}

/// Atomically write content to target within workspace boundary.
pub async fn atomic_write(workspace_root: &Path, path: &Path, content: &[u8]) -> Result<()> {
    let (canonical_root, mut canonical_target) =
        normalize_target_in_workspace(workspace_root, path)?;
    let parent = canonical_target
        .parent()
        .context("atomic_write target has no parent directory")?;
    fs::create_dir_all(parent).await?;

    let tmp_dir = ensure_atomic_tmp_dir(&canonical_root).await?;
    let tmp_path = tmp_dir.join(format!(
        ".{}.{}.tmp",
        canonical_target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("atomic"),
        Uuid::now_v7()
    ));

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&tmp_path).await?;
    file.write_all(content).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    run_atomic_write_test_hook();

    let (_, latest_target) = normalize_target_in_workspace(&canonical_root, path)?;
    canonical_target = latest_target;
    if let Some(parent) = canonical_target.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::rename(&tmp_path, &canonical_target).await?;

    #[cfg(unix)]
    if let Some(parent) = canonical_target.parent() {
        std::fs::File::open(parent)
            .with_context(|| format!("failed to open parent dir: {}", parent.display()))?
            .sync_all()
            .with_context(|| format!("failed to sync parent dir: {}", parent.display()))?;
    }
    Ok(())
}

/// Compute SHA-256 hex digest for an input string.
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Check whether raw debug logging is enabled for evolution internals.
pub fn is_raw_debug_enabled() -> bool {
    std::env::var(RAW_LOG_DEBUG_ENV)
        .or_else(|_| std::env::var(RAW_LOG_DEBUG_ENV_LEGACY))
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

/// RAII lock-file guard used by JSONL mutation paths.
#[derive(Debug)]
pub struct FileLockGuard {
    lock_path: PathBuf,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.lock_path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    path = %self.lock_path.display(),
                    error = %err,
                    "failed to remove file lock"
                );
            }
        }
    }
}

/// Acquire an exclusive lock-file beside `path` with bounded wait.
pub async fn acquire_file_lock(path: &Path) -> Result<FileLockGuard> {
    let parent = path
        .parent()
        .context("lock target has no parent directory")?;
    fs::create_dir_all(parent).await?;
    let lock_path = path.with_extension(format!(
        "{}.lock",
        path.extension().and_then(|v| v.to_str()).unwrap_or("file")
    ));

    let start = Instant::now();
    loop {
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
            .await
        {
            Ok(mut lock_file) => {
                lock_file.write_all(b"lock").await?;
                lock_file.flush().await?;
                return Ok(FileLockGuard { lock_path });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if start.elapsed() > Duration::from_secs(5) {
                    bail!("timed out waiting for file lock: {}", path.display());
                }
                sleep(Duration::from_millis(20)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn validate_path_rejects_parent_dir() {
        let dir = tempdir().unwrap();
        let err = validate_path_in_workspace(dir.path(), Path::new("../escape.txt")).unwrap_err();
        assert!(err.to_string().contains("parent traversal"));
    }

    #[tokio::test]
    async fn validate_path_rejects_absolute_path() {
        let dir = tempdir().unwrap();
        let abs = dir.path().join("x.txt").to_string_lossy().to_string();
        let err = validate_path_in_workspace(dir.path(), Path::new(&abs)).unwrap_err();
        assert!(err.to_string().contains("absolute paths"));
    }

    #[tokio::test]
    async fn atomic_write_replaces_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        atomic_write(dir.path(), &path, b"v1").await.unwrap();
        atomic_write(dir.path(), &path, b"v2").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), "v2");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_rejects_symlink_swap_race() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = dir.path().join("nested").join("victim.txt");
        fs::create_dir_all(target.parent().unwrap()).await.unwrap();

        let nested = target.parent().unwrap().to_path_buf();
        let outside_link = outside.path().join("escaped");
        set_atomic_write_test_hook(Some(Box::new(move || {
            let _ = std::fs::remove_dir_all(&nested);
            symlink(&outside_link, &nested).unwrap();
        })));

        let err = atomic_write(dir.path(), &target, b"nope")
            .await
            .unwrap_err();
        set_atomic_write_test_hook(None);
        assert!(!err.to_string().is_empty());
        assert!(!outside.path().join("escaped").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_rejects_symlinked_atomic_tmp_dir() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let evolution_dir = dir.path().join(".evolution");
        symlink(outside.path(), &evolution_dir).unwrap();

        let target = dir.path().join("safe.txt");
        let err = atomic_write(dir.path(), &target, b"payload")
            .await
            .unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("outside workspace")
                || err_msg.contains("atomic tmp dir")
                || err_msg.contains("escapes workspace"),
            "unexpected error: {err_msg}"
        );
        assert!(!outside.path().join(".atomic_tmp").exists());
    }
}
