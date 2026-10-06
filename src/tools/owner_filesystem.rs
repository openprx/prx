//! Descriptor-confined storage for an authenticated owner's logical files.
//!
//! Construction is reserved to audited runtime callers: supply trusted source
//! identity, never arguments supplied by the model. Approval and execution must
//! retain the same scope and use `resolve` for the same logical path.
#[cfg(unix)]
use anyhow::Context;
#[cfg(not(unix))]
use anyhow::bail;
use anyhow::ensure;
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug)]
pub(crate) struct OwnerFileTarget {
    namespace: String,
    logical: PathBuf,
    namespace_root: PathBuf,
    operation_target: PathBuf,
}

impl OwnerFileTarget {
    pub(crate) fn operation_target(&self) -> &Path {
        &self.operation_target
    }
    pub(crate) fn namespace_root(&self) -> &Path {
        &self.namespace_root
    }
    #[cfg(test)]
    fn operation_scope(&self) -> &str {
        &self.namespace
    }
}

pub(crate) struct OwnerFilesystem {
    identity: [u8; 32],
    namespace: String,
    namespace_root: PathBuf,
    #[cfg(unix)]
    root: std::os::fd::OwnedFd,
}

impl std::fmt::Debug for OwnerFilesystem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OwnerFilesystem").finish_non_exhaustive()
    }
}

impl PartialEq for OwnerFilesystem {
    fn eq(&self, other: &Self) -> bool {
        self.namespace == other.namespace
    }
}

impl Eq for OwnerFilesystem {}

impl OwnerFilesystem {
    /// Opens an existing application-owned config directory without mutation.
    /// The caller must ensure this directory cannot be reached by shared tools.
    pub(crate) fn open(
        app_private_root: &Path,
        workspace_id: &str,
        owner_id: &str,
        shared_workspace: &Path,
    ) -> anyhow::Result<Self> {
        ensure!(
            !workspace_id.is_empty() && !owner_id.is_empty(),
            "missing owner filesystem identity"
        );
        let identity = owner_identity(workspace_id, owner_id);
        let mut hash = Sha256::new();
        hash.update(b"prx.owner-filesystem.root.v1\0");
        hash.update(identity);
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};
            let root = rustix::fs::open(
                app_private_root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .context("opening owner filesystem root")?;
            let canonical = app_private_root
                .canonicalize()
                .context("resolving owner filesystem root")?;
            let workspace = shared_workspace.canonicalize().context("resolving shared workspace")?;
            ensure!(
                !canonical.join("owner-files").starts_with(&workspace)
                    && !workspace.starts_with(canonical.join("owner-files")),
                "owner filesystem and shared workspace overlap"
            );
            let reopened = rustix::fs::open(
                &canonical,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let stat = rustix::fs::fstat(&root)?;
            ensure!(
                stat.st_uid == effective_uid() && stat.st_mode & 0o022 == 0,
                "owner filesystem config root is not application-owned"
            );
            let resolved_stat = rustix::fs::fstat(&reopened)?;
            ensure!(
                stat.st_dev == resolved_stat.st_dev && stat.st_ino == resolved_stat.st_ino,
                "owner filesystem root changed"
            );
            // Root replacement changes the namespace and hence the operation
            // identity. The retained descriptor supplies authority for I/O.
            use std::os::unix::ffi::OsStrExt;
            let root_bytes = canonical.as_os_str().as_bytes();
            hash.update((root_bytes.len() as u64).to_be_bytes());
            hash.update(root_bytes);
            hash.update(stat.st_dev.to_be_bytes());
            hash.update(stat.st_ino.to_be_bytes());
            let namespace = hex::encode(hash.finalize());
            let namespace_root = canonical.join("owner-files").join("v1").join(&namespace);
            Ok(Self {
                identity,
                namespace,
                namespace_root,
                root,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (app_private_root, shared_workspace, hash);
            bail!("owner filesystem requires descriptor-relative filesystem support")
        }
    }

    #[cfg(test)]
    pub(crate) fn namespace_root(&self) -> &Path {
        &self.namespace_root
    }

    pub(crate) fn matches(&self, workspace_id: &str, owner_id: &str) -> bool {
        !workspace_id.is_empty() && !owner_id.is_empty() && self.identity == owner_identity(workspace_id, owner_id)
    }

    /// Computes an approval target without creating directories or files.
    pub(crate) fn resolve(&self, logical: &str) -> anyhow::Result<OwnerFileTarget> {
        ensure!(
            !logical.is_empty() && !logical.contains('\0') && !logical.contains('\\') && !logical.contains(':'),
            "invalid owner file path"
        );
        // Inspect the text too: Path::components normalizes repeated separators
        // and interior dot segments, which must not create approval aliases.
        ensure!(
            logical
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
            "invalid owner file path"
        );
        ensure!(
            logical.len() <= 4096
                && logical.split('/').count() <= 256
                && logical.split('/').all(|part| part.len() <= 255),
            "owner file path exceeds limits"
        );
        let path = Path::new(logical);
        ensure!(
            path.components().all(|part| matches!(part, Component::Normal(_))),
            "owner file path must be relative"
        );
        let target = OwnerFileTarget {
            namespace: self.namespace.clone(),
            logical: path.to_path_buf(),
            namespace_root: self.namespace_root.clone(),
            operation_target: self.namespace_root.join(path),
        };
        self.validate_existing(&target, true)?;
        Ok(target)
    }

    pub(crate) fn resolve_existing(&self, logical: &str) -> anyhow::Result<OwnerFileTarget> {
        let target = self.resolve(logical)?;
        self.validate_existing(&target, false)?;
        Ok(target)
    }

    fn validate_existing(&self, target: &OwnerFileTarget, allow_missing: bool) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            let result = (|| -> anyhow::Result<()> {
                let (parent, name) = self.parent(target, false)?;
                if allow_missing {
                    check_existing(&parent, name)
                } else {
                    open_regular(&parent, name).map(|_| ())
                }
            })();
            match result {
                Err(error)
                    if allow_missing
                        && error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) =>
                {
                    Ok(())
                }
                result => result,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (target, allow_missing);
            bail!("owner filesystem is unsupported")
        }
    }

    fn validate_target(&self, target: &OwnerFileTarget) -> anyhow::Result<()> {
        ensure!(target.namespace == self.namespace, "owner filesystem scope mismatch");
        Ok(())
    }

    pub(crate) fn read(&self, target: &OwnerFileTarget, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
        self.validate_target(target)?;
        #[cfg(unix)]
        {
            use std::io::Read;
            let (parent, name) = self.parent(target, false)?;
            let file = open_regular(&parent, name)?;
            ensure!(
                file.metadata()?.len() <= 10 * 1024 * 1024,
                "owner file exceeds read size limit"
            );
            let mut bytes = Vec::new();
            file.take(max_bytes.min(10 * 1024 * 1024 + 1)).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 10 * 1024 * 1024, "owner file exceeds read size limit");
            Ok(bytes)
        }
        #[cfg(not(unix))]
        {
            let _ = max_bytes;
            bail!("owner filesystem is unsupported")
        }
    }

    /// Call only after exact target authorization. The backend does not issue
    /// grants or interpret model arguments as authority.
    pub(crate) fn write_authorized(&self, target: &OwnerFileTarget, content: &[u8]) -> anyhow::Result<()> {
        self.validate_target(target)?;
        #[cfg(unix)]
        {
            let (parent, name) = self.parent(target, true)?;
            check_existing(&parent, name)?;
            replace_file(&parent, name, content)
        }
        #[cfg(not(unix))]
        {
            let _ = content;
            bail!("owner filesystem is unsupported")
        }
    }

    /// Replaces exact UTF-8 text in an existing file after authorization.
    pub(crate) fn edit_authorized(
        &self,
        target: &OwnerFileTarget,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> anyhow::Result<usize> {
        self.validate_target(target)?;
        ensure!(!old.is_empty() && old != new, "invalid owner file edit");
        #[cfg(unix)]
        {
            use std::io::Read;
            let (parent, name) = self.parent(target, false)?;
            let mut file = open_regular(&parent, name)?;
            let original = rustix::fs::fstat(&file)?;
            // Bound owner-edit memory independently of file size.
            ensure!(
                original.st_size >= 0 && original.st_size <= 10 * 1024 * 1024,
                "owner file exceeds edit size limit"
            );
            let mut text = String::new();
            (&mut file).take(10 * 1024 * 1024 + 1).read_to_string(&mut text)?;
            ensure!(text.len() <= 10 * 1024 * 1024, "owner file exceeds edit size limit");
            let count = text.matches(old).count();
            ensure!(
                count > 0 && (replace_all || count == 1),
                "owner file edit requires an unambiguous match"
            );
            let replacement = if replace_all {
                text.replace(old, new)
            } else {
                text.replacen(old, new, 1)
            };
            let current = rustix::fs::statat(&parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
            ensure!(
                current.st_dev == original.st_dev
                    && current.st_ino == original.st_ino
                    && current.st_size == original.st_size
                    && current.st_mtime == original.st_mtime,
                "owner file changed during edit"
            );
            check_existing(&parent, name)?;
            replace_file(&parent, name, replacement.as_bytes())?;
            Ok(count)
        }
        #[cfg(not(unix))]
        {
            let _ = replace_all;
            bail!("owner filesystem is unsupported")
        }
    }

    #[cfg(unix)]
    fn parent<'a>(
        &self,
        target: &'a OwnerFileTarget,
        create: bool,
    ) -> anyhow::Result<(std::os::fd::OwnedFd, &'a std::ffi::OsStr)> {
        let mut directory = open_directory(&self.root, std::ffi::OsStr::new("owner-files"), create)?;
        directory = open_directory(&directory, std::ffi::OsStr::new("v1"), create)?;
        directory = open_directory(&directory, std::ffi::OsStr::new(&self.namespace), create)?;
        let name = target.logical.file_name().context("owner file name missing")?;
        if let Some(parent) = target.logical.parent() {
            for component in parent.components() {
                directory = open_directory(&directory, component.as_os_str(), create)?;
            }
        }
        Ok((directory, name))
    }
}

#[cfg(unix)]
// Group and other rwx permission bits occupy the lowest six mode bits.
fn open_directory(
    parent: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
    create: bool,
) -> anyhow::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let directory = match rustix::fs::openat(parent, name, flags, Mode::empty()) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT) if create => {
            match rustix::fs::mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
            rustix::fs::openat(parent, name, flags, Mode::empty())?
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        {
            let stat = rustix::fs::fstat(&directory)?;
            stat.st_mode.trailing_zeros() >= 6 && stat.st_uid == effective_uid()
        },
        "owner directory is not private"
    );
    Ok(directory)
}

#[cfg(unix)]
fn open_regular(parent: &std::os::fd::OwnedFd, name: &std::ffi::OsStr) -> anyhow::Result<std::fs::File> {
    use rustix::fs::{FileType, Mode, OFlags};
    let file = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let stat = rustix::fs::fstat(&file)?;
    ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
            && stat.st_nlink == 1
            && stat.st_mode.trailing_zeros() >= 6
            && stat.st_uid == effective_uid(),
        "owner file must be regular and single-link"
    );
    Ok(std::fs::File::from(file))
}

#[cfg(unix)]
fn check_existing(parent: &std::os::fd::OwnedFd, name: &std::ffi::OsStr) -> anyhow::Result<()> {
    use rustix::fs::{AtFlags, FileType};
    match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => ensure!(
            FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                && stat.st_nlink == 1
                && stat.st_mode.trailing_zeros() >= 6
                && stat.st_uid == effective_uid(),
            "owner file must be regular and single-link"
        ),
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn replace_file(parent: &std::os::fd::OwnedFd, name: &std::ffi::OsStr, content: &[u8]) -> anyhow::Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    use std::io::Write;
    let temporary = format!(".prx-write-{}", uuid::Uuid::new_v4());
    let fd = rustix::fs::openat(
        parent,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::File::from(fd);
        file.write_all(content)?;
        file.sync_all()?;
        check_existing(parent, name)?;
        rustix::fs::renameat(parent, temporary.as_str(), parent, name)?;
        Ok(())
    })();
    if result.is_err() {
        // Cleanup is descriptor-relative and cannot follow an injected symlink.
        let _ = rustix::fs::unlinkat(parent, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn owner_identity(workspace_id: &str, owner_id: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"prx.owner-filesystem.identity.v1\0");
    for value in [workspace_id, owner_id] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    hash.finalize().into()
}

#[cfg(all(test, unix))]
mod fifo_fixture;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn roots() -> std::io::Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let config = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&config)?;
        std::fs::create_dir(&workspace)?;
        Ok((temp, config, workspace))
    }

    #[test]
    fn resolving_missing_owner_target_creates_nothing_and_rejects_aliases() {
        let (_temp, config, workspace) = roots().unwrap();
        let scope = OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).unwrap();
        scope.resolve("nested/file.txt").unwrap();
        assert!(!config.join("owner-files").exists());
        for path in [
            "",
            "/file",
            "../file",
            "nested/../file",
            "./file",
            "a//b",
            "a/",
            "a\\b",
            "C:/file",
            "C:file",
        ] {
            assert!(scope.resolve(path).is_err(), "accepted {path}");
        }
    }

    #[test]
    fn owner_and_workspace_scopes_cannot_read_or_write_each_other() {
        let (_temp, config, workspace) = roots().unwrap();
        let scope = OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).unwrap();
        let other = OwnerFilesystem::open(&config, "workspace_a", "owner_b", &workspace).unwrap();
        let another_workspace = OwnerFilesystem::open(&config, "workspace_b", "owner_a", &workspace).unwrap();
        let target = scope.resolve("nested/file.txt").unwrap();
        scope.write_authorized(&target, b"private").unwrap();
        assert_eq!(scope.read(&target, 100).unwrap(), b"private");
        assert!(other.read(&target, 100).is_err());
        assert!(another_workspace.write_authorized(&target, b"changed").is_err());
        assert!(!workspace.join("nested").exists());
        assert_eq!(scope.edit_authorized(&target, "private", "updated", false).unwrap(), 1);
        assert_eq!(scope.read(&target, 100).unwrap(), b"updated");
    }

    #[test]
    fn symlink_and_hardlink_targets_are_denied_without_changing_external_file() {
        let (temp, config, workspace) = roots().unwrap();
        let scope = OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).unwrap();
        let target = scope.resolve("file.txt").unwrap();
        scope.write_authorized(&target, b"private").unwrap();
        let external = temp.path().join("external");
        std::fs::write(&external, b"external").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(target.operation_target()).unwrap();
        symlink(&external, target.operation_target()).unwrap();
        assert!(scope.resolve("file.txt").is_err());
        assert!(scope.write_authorized(&target, b"changed").is_err());
        std::fs::remove_file(target.operation_target()).unwrap();
        std::fs::hard_link(&external, target.operation_target()).unwrap();
        assert!(scope.read(&target, 100).is_err());
        assert!(scope.write_authorized(&target, b"changed").is_err());
        assert_eq!(std::fs::read(&external).unwrap(), b"external");
    }

    #[test]
    fn symlink_parent_and_shared_root_overlap_are_denied() {
        let (_temp, config, workspace) = roots().unwrap();
        assert!(OwnerFilesystem::open(&workspace, "workspace_a", "owner_a", &workspace).is_err());
        let scope = OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).unwrap();
        let target = scope.resolve("seed").unwrap();
        scope.write_authorized(&target, b"seed").unwrap();
        symlink(&workspace, target.namespace_root().join("escape")).unwrap();
        assert!(scope.resolve("escape/file").is_err());
    }

    #[test]
    fn default_workspace_beside_private_storage_is_supported() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        assert!(OwnerFilesystem::open(temp.path(), "workspace_a", "owner_a", &workspace).is_ok());
    }

    #[test]
    fn writable_config_and_nonprivate_leaf_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, config, workspace) = roots().unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).is_err());
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
        let scope = OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).unwrap();
        let target = scope.resolve("file").unwrap();
        scope.write_authorized(&target, b"private").unwrap();
        std::fs::set_permissions(target.operation_target(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(scope.resolve("file").is_err());
        assert!(scope.read(&target, 100).is_err());
        assert!(scope.write_authorized(&target, b"changed").is_err());
    }

    #[test]
    fn nonregular_targets_and_symlink_root_fail_closed() {
        let (temp, config, workspace) = roots().unwrap();
        let alias = temp.path().join("config_alias");
        symlink(&config, &alias).unwrap();
        assert!(OwnerFilesystem::open(&alias, "workspace_a", "owner_a", &workspace).is_err());
        let scope = OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).unwrap();
        let target = scope.resolve("file").unwrap();
        scope.write_authorized(&target, b"private").unwrap();
        std::fs::remove_file(target.operation_target()).unwrap();
        std::fs::create_dir(target.operation_target()).unwrap();
        assert!(scope.resolve("file").is_err());
        assert!(scope.read(&target, 100).is_err());
        assert!(scope.write_authorized(&target, b"changed").is_err());
        std::fs::remove_dir(target.operation_target()).unwrap();
        assert!(fifo_fixture::create_fifo(target.operation_target()).unwrap().success());
        assert!(scope.resolve("file").is_err());
        assert!(scope.read(&target, 100).is_err());
        assert!(scope.write_authorized(&target, b"changed").is_err());
    }

    #[test]
    fn read_limit_applies_to_file_size_even_when_requested_prefix_is_small() {
        let (_temp, config, workspace) = roots().unwrap();
        let scope = OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).unwrap();
        let target = scope.resolve("file").unwrap();
        scope.write_authorized(&target, b"private").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(target.operation_target())
            .unwrap()
            .set_len(10 * 1024 * 1024 + 1)
            .unwrap();
        assert!(scope.read(&target, 1).is_err());
        assert!(scope.resolve(&"a".repeat(256)).is_err());
        assert!(
            scope
                .resolve(&std::iter::repeat_n("a", 257).collect::<Vec<_>>().join("/"))
                .is_err()
        );
    }

    #[test]
    fn root_replacement_changes_approval_identity_while_existing_scope_stays_pinned() {
        let (_temp, config, workspace) = roots().unwrap();
        let original = OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).unwrap();
        let target = original.resolve("file").unwrap();
        std::fs::rename(&config, config.with_extension("previous")).unwrap();
        std::fs::create_dir(&config).unwrap();
        let replacement = OwnerFilesystem::open(&config, "workspace_a", "owner_a", &workspace).unwrap();
        assert_ne!(
            target.operation_scope(),
            replacement.resolve("file").unwrap().operation_scope()
        );
        assert!(replacement.write_authorized(&target, b"changed").is_err());
    }
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}
