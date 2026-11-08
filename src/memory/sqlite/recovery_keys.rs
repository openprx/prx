//! Descriptor-relative, no-follow recovery key storage. Never repairs keys.
#[cfg(unix)]
use anyhow::Context;
use std::path::Path;
#[cfg(unix)]
use std::{
    fs::File,
    io::{Read, Write},
    path::Component,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// Name of the key directory inside a memory directory.
pub(super) const KEYS_DIRECTORY: &str = ".recovery_keys";

/// Marker file of an unpublished backup staging directory. A directory that
/// carries it is never opened as a key repository, so a crash leftover can be
/// neither restored nor turned into a fresh authority.
pub(super) const INCOMPLETE_MARKER: &str = ".backup-incomplete";

pub(super) struct KeyDirectory(#[cfg(unix)] File);

impl KeyDirectory {
    #[cfg(unix)]
    pub(super) fn open(memory: &Path, create: bool) -> anyhow::Result<Self> {
        Self::open_with(memory, create, false)
    }

    /// Open (creating when absent) the key directory of an unpublished backup
    /// staging directory, which is the only place the incomplete marker is allowed.
    #[cfg(unix)]
    pub(super) fn open_staging(staging: &Path) -> anyhow::Result<Self> {
        Self::open_with(staging, true, true)
    }

    #[cfg(unix)]
    fn open_with(memory: &Path, create: bool, staging: bool) -> anyhow::Result<Self> {
        use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, open, openat, statat};
        anyhow::ensure!(memory.is_absolute(), "recovery path must be absolute");
        let owner = rustix::process::geteuid().as_raw();
        let mut directory = File::from(open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        #[cfg(test)]
        check_cloexec(&directory)?;
        for component in memory.components() {
            let name = match component {
                Component::RootDir => continue,
                Component::Normal(name) => name,
                _ => anyhow::bail!("unsafe recovery ancestor"),
            };
            directory = File::from(openat(
                &directory,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            #[cfg(test)]
            check_cloexec(&directory)?;
            let metadata = directory.metadata()?;
            // Root-owned sticky temporary roots are safe ancestors, but never
            // acceptable as the repository/key directory itself.
            let sticky_root = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
            anyhow::ensure!(
                (metadata.uid() == owner || metadata.uid() == 0) && (metadata.mode() & 0o022 == 0 || sticky_root),
                "unsafe recovery ancestor ownership or mode"
            );
        }
        let metadata = directory.metadata()?;
        anyhow::ensure!(
            metadata.uid() == owner && metadata.mode() & 0o777 == 0o700,
            "unsafe memory directory"
        );
        if !staging {
            match statat(&directory, INCOMPLETE_MARKER, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => anyhow::bail!("incomplete recovery backup"),
                Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if create {
            match mkdirat(&directory, KEYS_DIRECTORY, Mode::from_raw_mode(0o700)) {
                Ok(()) => directory.sync_all()?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let keys = File::from(openat(
            &directory,
            KEYS_DIRECTORY,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        #[cfg(test)]
        check_cloexec(&keys)?;
        let metadata = keys.metadata()?;
        anyhow::ensure!(
            metadata.uid() == owner && metadata.permissions().mode() & 0o777 == 0o700,
            "unsafe recovery key directory"
        );
        // Sync even an existing directory: a previous failed initializer may
        // have created it but not persisted the parent directory entry.
        directory.sync_all()?;
        Ok(Self(keys))
    }

    #[cfg(not(unix))]
    pub(super) fn open(_memory: &Path, _create: bool) -> anyhow::Result<Self> {
        anyhow::bail!("secure recovery key storage unavailable on this platform")
    }

    #[cfg(not(unix))]
    pub(super) fn open_staging(_staging: &Path) -> anyhow::Result<Self> {
        anyhow::bail!("secure recovery key storage unavailable on this platform")
    }

    #[cfg(unix)]
    fn filename(id: &str) -> anyhow::Result<String> {
        anyhow::ensure!(uuid::Uuid::parse_str(id)?.to_string() == id, "invalid recovery key id");
        Ok(format!("{id}.key"))
    }

    pub(super) fn key_id(bytes: &[u8; 32]) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
        let mut prefix = [0u8; 16];
        for (destination, source) in prefix.iter_mut().zip(digest.as_ref()) {
            *destination = *source;
        }
        uuid::Uuid::from_bytes(prefix).to_string()
    }

    pub(super) fn create(&self) -> anyhow::Result<String> {
        use ring::rand::SecureRandom;
        let mut bytes = [0u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("recovery key generation failed"))?;
        let id = Self::key_id(&bytes);
        self.install(&id, &bytes)?;
        Ok(id)
    }

    #[cfg(unix)]
    pub(super) fn install(&self, id: &str, bytes: &[u8; 32]) -> anyhow::Result<()> {
        use rustix::fs::{Mode, OFlags, openat};
        anyhow::ensure!(Self::key_id(bytes) == id, "recovery key identity mismatch");
        let mut file = File::from(openat(
            &self.0,
            Self::filename(id)?.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        #[cfg(test)]
        check_cloexec(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        self.0.sync_all()?;
        Ok(())
    }

    #[cfg(not(unix))]
    pub(super) fn install(&self, _id: &str, _bytes: &[u8; 32]) -> anyhow::Result<()> {
        anyhow::bail!("secure recovery keys unavailable")
    }

    #[cfg(unix)]
    pub(super) fn load(&self, id: &str) -> anyhow::Result<ring::hmac::Key> {
        let bytes = self.read(id)?;
        Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &bytes))
    }

    /// The checked bytes of one key, for the backup path only. The caller must
    /// hand them straight to `install` and never keep or log them.
    #[cfg(unix)]
    pub(super) fn read(&self, id: &str) -> anyhow::Result<[u8; 32]> {
        use rustix::fs::{Mode, OFlags, openat};
        let mut file = File::from(openat(
            &self.0,
            Self::filename(id)?.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        #[cfg(test)]
        check_cloexec(&file)?;
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file()
                && metadata.uid() == rustix::process::geteuid().as_raw()
                && metadata.mode() & 0o777 == 0o600
                && metadata.len() == 32
                && metadata.nlink() == 1,
            "unsafe or malformed recovery key"
        );
        let mut bytes = [0; 32];
        file.read_exact(&mut bytes).context("read recovery key")?;
        anyhow::ensure!(Self::key_id(&bytes) == id, "corrupt established recovery key");
        file.sync_all()?;
        self.0.sync_all()?;
        Ok(bytes)
    }

    #[cfg(not(unix))]
    pub(super) fn load(&self, _id: &str) -> anyhow::Result<ring::hmac::Key> {
        anyhow::bail!("secure recovery keys unavailable")
    }

    #[cfg(not(unix))]
    pub(super) fn read(&self, _id: &str) -> anyhow::Result<[u8; 32]> {
        anyhow::bail!("secure recovery keys unavailable")
    }
}

// Check the actual descriptors while they are live, including transient walk,
// install and load descriptors that are closed before returning to the caller.
#[cfg(all(test, unix))]
fn check_cloexec(file: &File) -> anyhow::Result<()> {
    anyhow::ensure!(
        rustix::io::fcntl_getfd(file)?.contains(rustix::io::FdFlags::CLOEXEC),
        "recovery descriptor must be close-on-exec"
    );
    Ok(())
}
