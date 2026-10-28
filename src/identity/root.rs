//! Managed main-role identity with a read-compatible legacy workspace layout.
use super::scaffold::{ProjectContext, scaffold_workspace};
use anyhow::{Context, Result, ensure};
use std::io::Write;
use std::path::{Path, PathBuf};

const ROLE_FILES: [&str; 4] = ["SOUL.md", "AGENTS.md", "IDENTITY.md", "THINKING.md"];
const WORKSPACE_FILES: [&str; 5] = ["USER.md", "TOOLS.md", "MEMORY.md", "BOOTSTRAP.md", "HEARTBEAT.md"];
const MARKER: &str = ".prx-identity";

pub fn file_directory(workspace: &Path, filename: &str) -> PathBuf {
    let main = workspace.join("identities/main");
    if ROLE_FILES.contains(&filename) && std::fs::read_to_string(main.join(MARKER)).ok().as_deref() == Some("1\n") {
        main
    } else {
        workspace.to_path_buf()
    }
}

/// Publish a complete main identity directory atomically. Legacy role files
/// are copied verbatim and retained as migration backups, never overwritten.
pub fn initialize(workspace: &Path, context: &ProjectContext) -> Result<()> {
    std::fs::create_dir_all(workspace)?;
    let templates = tempfile::tempdir()?;
    scaffold_workspace(templates.path(), context)?;
    for name in WORKSPACE_FILES {
        copy_if_absent(&templates.path().join(name), &workspace.join(name))?;
    }
    for directory in ["sessions", "memory", "state", "cron", "skills"] {
        std::fs::create_dir_all(workspace.join(directory))?;
    }
    let identities = workspace.join("identities");
    if let Ok(metadata) = std::fs::symlink_metadata(&identities) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Managed identities root cannot be a symlink"
        );
    }
    std::fs::create_dir_all(&identities)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(identities.join(".main-init.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let main = identities.join("main");
    if let Ok(metadata) = std::fs::symlink_metadata(&main) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Main identity must be a directory"
        );
        ensure!(
            std::fs::read_to_string(main.join(MARKER)).ok().as_deref() == Some("1\n"),
            "Existing identities/main is unmanaged; resolve the conflict before initializing"
        );
        for name in ROLE_FILES {
            copy_if_absent(&templates.path().join(name), &main.join(name))?;
        }
        return Ok(());
    }
    let staging = tempfile::Builder::new().prefix(".main-").tempdir_in(&identities)?;
    for name in ROLE_FILES {
        let legacy = workspace.join(name);
        let source = if legacy.exists() {
            legacy
        } else {
            templates.path().join(name)
        };
        copy_if_absent(&source, &staging.path().join(name))?;
    }
    std::fs::write(staging.path().join(MARKER), "1\n")?;
    // The cross-process lock serializes managed initializers. The marker and
    // all role files become visible together through a directory rename.
    std::fs::rename(staging.path(), &main).context("Cannot publish main identity")?;
    tracing::info!(path = %main.display(), "main role identity selected; any legacy root role files are retained as backups");
    Ok(())
}

fn copy_if_absent(source: &Path, target: &Path) -> Result<()> {
    let content = std::fs::read(source)?;
    let mut file = tempfile::NamedTempFile::new_in(target.parent().context("Identity file has no parent")?)?;
    file.write_all(&content)?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(target) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_preserves_legacy_persona_and_keeps_user_memory_workspace_scoped() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("SOUL.md"), "Legacy persona sentinel").unwrap();
        initialize(root.path(), &ProjectContext::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("identities/main/SOUL.md")).unwrap(),
            "Legacy persona sentinel"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("SOUL.md")).unwrap(),
            "Legacy persona sentinel"
        );
        assert_eq!(file_directory(root.path(), "MEMORY.md"), root.path());
        assert_eq!(
            file_directory(root.path(), "SOUL.md"),
            root.path().join("identities/main")
        );
        std::fs::write(root.path().join("identities/main/SOUL.md"), "Managed persona sentinel").unwrap();
        std::fs::remove_file(root.path().join("identities/main/IDENTITY.md")).unwrap();
        initialize(root.path(), &ProjectContext::default()).unwrap();
        assert!(root.path().join("identities/main/IDENTITY.md").is_file());
        assert_eq!(
            std::fs::read_to_string(root.path().join("identities/main/SOUL.md")).unwrap(),
            "Managed persona sentinel"
        );
    }
}
