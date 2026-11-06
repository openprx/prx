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

pub(super) struct KeyDirectory(#[cfg(unix)] File);

impl KeyDirectory {
    #[cfg(unix)]
    pub(super) fn open(memory: &Path, create: bool) -> anyhow::Result<Self> {
        use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
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
        if create {
            match mkdirat(&directory, ".recovery_keys", Mode::from_raw_mode(0o700)) {
                Ok(()) => directory.sync_all()?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let keys = File::from(openat(
            &directory,
            ".recovery_keys",
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

    #[cfg(unix)]
    fn filename(id: &str) -> anyhow::Result<String> {
        anyhow::ensure!(uuid::Uuid::parse_str(id)?.to_string() == id, "invalid recovery key id");
        Ok(format!("{id}.key"))
    }

    fn key_id(bytes: &[u8; 32]) -> String {
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
        Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &bytes))
    }

    #[cfg(not(unix))]
    pub(super) fn load(&self, _id: &str) -> anyhow::Result<ring::hmac::Key> {
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
