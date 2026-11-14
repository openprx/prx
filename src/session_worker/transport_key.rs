//! The strict session-worker transport secret `T`.
//!
//! `T` signs the worker capability and derives every ordinary/operator child key
//! (`K_child`). It is a pure function of the `SESSION_WORKER_SECRET` environment
//! override (exactly 32 bytes) or of one persisted 32-byte file that PRX writes
//! once and never rewrites, chmods, rotates or deletes. There is no fallback key:
//! any failure refuses the launch or the verification.
use std::io;
use std::path::{Path, PathBuf};

/// Environment override for the transport secret (exactly 32 bytes).
pub(crate) const SESSION_WORKER_SECRET_ENV: &str = "SESSION_WORKER_SECRET";
/// Explicit path override for the persisted secret file.
pub(crate) const SESSION_WORKER_SECRET_PATH_ENV: &str = "OPENPRX_SESSION_WORKER_SECRET_PATH";
const SECRET_FILE: &str = "session_worker.secret";
const SECRET_LEN: usize = 32;

/// Why the transport secret could not be produced. Every variant refuses.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportSecretError {
    #[error("session-worker transport secret unavailable: {0}")]
    TransportSecretUnavailable(String),
    #[error("session-worker transport secret invalid: {0}")]
    TransportSecretInvalid(&'static str),
    #[error("session-worker transport secret unsafe: {0}")]
    TransportSecretUnsafe(&'static str),
}

/// The transport secret bytes. Never logged or displayed.
pub(crate) struct TransportSecret([u8; SECRET_LEN]);

impl std::fmt::Debug for TransportSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TransportSecret([REDACTED])")
    }
}

impl TransportSecret {
    /// Parent side, before every process, batch or operator launch: the env
    /// override, else the persisted file, created once if absent.
    pub(crate) fn for_launch() -> Result<Self, TransportSecretError> {
        if let Some(secret) = env_secret()? {
            return Ok(secret);
        }
        for_launch_at(&secret_path()?, generate_secret)
    }

    /// Worker start, parent restart and batch reads: the env override, else the
    /// existing file. Never creates or modifies anything.
    pub(crate) fn existing() -> Result<Self, TransportSecretError> {
        if let Some(secret) = env_secret()? {
            return Ok(secret);
        }
        existing_at(&secret_path()?)
    }

    /// The HMAC-SHA256 key over the secret bytes.
    pub(crate) fn hmac_key(&self) -> ring::hmac::Key {
        ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0)
    }
}

fn env_secret() -> Result<Option<TransportSecret>, TransportSecretError> {
    match std::env::var_os(SESSION_WORKER_SECRET_ENV) {
        None => Ok(None),
        Some(value) => {
            let bytes: [u8; SECRET_LEN] = value.as_encoded_bytes().try_into().map_err(|_| {
                TransportSecretError::TransportSecretInvalid("SESSION_WORKER_SECRET must be exactly 32 bytes")
            })?;
            Ok(Some(TransportSecret(bytes)))
        }
    }
}

/// Location of the persisted secret: the explicit override, else
/// `<config_dir>/keys/session_worker.secret`, else `~/.openprx/keys/…`.
pub(crate) fn secret_path() -> Result<PathBuf, TransportSecretError> {
    if let Some(explicit) = std::env::var_os(SESSION_WORKER_SECRET_PATH_ENV) {
        return Ok(PathBuf::from(explicit));
    }
    crate::config::process_config_dir()
        .or_else(crate::config::default_openprx_dir)
        .map(|dir| dir.join("keys").join(SECRET_FILE))
        .ok_or_else(|| TransportSecretError::TransportSecretUnavailable("no config directory".into()))
}

fn split_path(path: &Path) -> Result<(&Path, &std::ffi::OsStr), TransportSecretError> {
    let name = path.file_name().ok_or(TransportSecretError::TransportSecretInvalid(
        "secret path has no file name",
    ))?;
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok((dir, name))
}

fn unavailable(context: &str, error: impl std::fmt::Display) -> TransportSecretError {
    TransportSecretError::TransportSecretUnavailable(format!("{context}: {error}"))
}

#[cfg(unix)]
fn open_dir(dir: &Path) -> Result<std::fs::File, TransportSecretError> {
    use rustix::fs::{Mode, OFlags, open};
    use std::os::unix::fs::MetadataExt as _;
    let fd = open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| unavailable("open secret directory", error))?;
    let dir_file = std::fs::File::from(fd);
    let metadata = dir_file
        .metadata()
        .map_err(|error| unavailable("stat secret directory", error))?;
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(TransportSecretError::TransportSecretUnsafe(
            "secret directory not owned by the current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(TransportSecretError::TransportSecretUnsafe(
            "secret directory is group or world accessible",
        ));
    }
    Ok(dir_file)
}

/// Read an existing secret: `openat(O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC)`,
/// `fstat` on the descriptor, then at most 33 bytes. Never creates or chmods.
#[cfg(unix)]
pub(crate) fn existing_at(path: &Path) -> Result<TransportSecret, TransportSecretError> {
    use rustix::fs::{Mode, OFlags, openat};
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    let (dir, name) = split_path(path)?;
    let dir_file = std::fs::File::from(
        rustix::fs::open(dir, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|error| unavailable("open secret directory", error))?,
    );
    let file = std::fs::File::from(
        openat(
            &dir_file,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| {
            if error == rustix::io::Errno::LOOP {
                TransportSecretError::TransportSecretUnsafe("secret path is a symbolic link")
            } else {
                unavailable("open secret", error)
            }
        })?,
    );
    let metadata = file.metadata().map_err(|error| unavailable("stat secret", error))?;
    if !metadata.is_file() {
        return Err(TransportSecretError::TransportSecretUnsafe(
            "secret is not a regular file",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(TransportSecretError::TransportSecretUnsafe(
            "secret not owned by the current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(TransportSecretError::TransportSecretUnsafe(
            "secret is group or world accessible",
        ));
    }
    if metadata.nlink() != 1 {
        return Err(TransportSecretError::TransportSecretUnsafe(
            "secret has more than one link",
        ));
    }
    if metadata.len() != SECRET_LEN as u64 {
        return Err(TransportSecretError::TransportSecretInvalid(
            "secret file is not 32 bytes",
        ));
    }
    let mut bytes = Vec::with_capacity(SECRET_LEN + 1);
    file.take(SECRET_LEN as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unavailable("read secret", error))?;
    let secret: [u8; SECRET_LEN] = bytes
        .try_into()
        .map_err(|_| TransportSecretError::TransportSecretInvalid("secret file is not 32 bytes"))?;
    Ok(TransportSecret(secret))
}

#[cfg(not(unix))]
pub(crate) fn existing_at(_path: &Path) -> Result<TransportSecret, TransportSecretError> {
    Err(TransportSecretError::TransportSecretUnavailable(
        "secure transport secret storage unavailable on this platform".into(),
    ))
}

/// Create the secret once if absent (0600 tempfile, `fsync`, `persist_noclobber`,
/// directory `fsync`); on a lost race read the winner's file. Any error refuses.
#[cfg(unix)]
pub(crate) fn for_launch_at(
    path: &Path,
    make_secret: impl FnOnce() -> Result<[u8; SECRET_LEN], TransportSecretError>,
) -> Result<TransportSecret, TransportSecretError> {
    use std::io::Write as _;
    use std::os::unix::fs::DirBuilderExt as _;
    let (dir, _) = split_path(path)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| unavailable("create secret directory", error))?;
    let dir_file = open_dir(dir)?;
    match existing_at(path) {
        Ok(secret) => return Ok(secret),
        Err(TransportSecretError::TransportSecretUnavailable(_)) if !path_present(path) => {}
        Err(error) => return Err(error),
    }
    let secret = make_secret()?;
    let mut pending =
        tempfile::NamedTempFile::new_in(dir).map_err(|error| unavailable("create secret tempfile", error))?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        pending
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| unavailable("restrict secret tempfile", error))?;
    }
    pending
        .write_all(&secret)
        .map_err(|error| unavailable("write secret tempfile", error))?;
    pending
        .as_file()
        .sync_all()
        .map_err(|error| unavailable("sync secret tempfile", error))?;
    match pending.persist_noclobber(path) {
        Ok(_) => {
            dir_file
                .sync_all()
                .map_err(|error| unavailable("sync secret directory", error))?;
            Ok(TransportSecret(secret))
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => existing_at(path),
        Err(error) => Err(unavailable("publish secret", error.error)),
    }
}

#[cfg(not(unix))]
pub(crate) fn for_launch_at(
    _path: &Path,
    _make_secret: impl FnOnce() -> Result<[u8; SECRET_LEN], TransportSecretError>,
) -> Result<TransportSecret, TransportSecretError> {
    Err(TransportSecretError::TransportSecretUnavailable(
        "secure transport secret storage unavailable on this platform".into(),
    ))
}

#[cfg(unix)]
fn path_present(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

fn generate_secret() -> Result<[u8; SECRET_LEN], TransportSecretError> {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0u8; SECRET_LEN];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| TransportSecretError::TransportSecretUnavailable("system random generator failed".into()))?;
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{TransportSecretError, existing_at, for_launch_at};
    use std::os::unix::fs::PermissionsExt as _;

    fn private_dir() -> anyhow::Result<tempfile::TempDir> {
        let dir = tempfile::TempDir::new()?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }

    #[test]
    fn existing_refuses_fifo_without_blocking() -> anyhow::Result<()> {
        let dir = private_dir()?;
        let path = dir.path().join("session_worker.secret");
        rustix::fs::mkfifoat(rustix::fs::CWD, &path, rustix::fs::Mode::from_raw_mode(0o600))?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sender.send(existing_at(&path).map(|_| ()));
        });
        let outcome = receiver.recv_timeout(std::time::Duration::from_secs(2))?;
        assert!(
            matches!(outcome, Err(TransportSecretError::TransportSecretUnsafe(_))),
            "{outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn existing_refuses_symlink_widened_mode_and_wrong_length() -> anyhow::Result<()> {
        let dir = private_dir()?;
        let real = dir.path().join("real.secret");
        std::fs::write(&real, [7u8; 32])?;
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600))?;
        assert!(existing_at(&real).is_ok());

        let link = dir.path().join("link.secret");
        std::os::unix::fs::symlink(&real, &link)?;
        assert!(matches!(
            existing_at(&link),
            Err(TransportSecretError::TransportSecretUnsafe(_))
        ));

        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644))?;
        assert!(matches!(
            existing_at(&real),
            Err(TransportSecretError::TransportSecretUnsafe(_))
        ));
        // A read path never chmods.
        assert_eq!(std::fs::metadata(&real)?.permissions().mode() & 0o777, 0o644);

        let short = dir.path().join("short.secret");
        std::fs::write(&short, [7u8; 31])?;
        std::fs::set_permissions(&short, std::fs::Permissions::from_mode(0o600))?;
        assert!(matches!(
            existing_at(&short),
            Err(TransportSecretError::TransportSecretInvalid(_))
        ));
        let long = dir.path().join("long.secret");
        std::fs::write(&long, [7u8; 33])?;
        std::fs::set_permissions(&long, std::fs::Permissions::from_mode(0o600))?;
        assert!(matches!(
            existing_at(&long),
            Err(TransportSecretError::TransportSecretInvalid(_))
        ));
        assert!(matches!(
            existing_at(&dir.path().join("absent.secret")),
            Err(TransportSecretError::TransportSecretUnavailable(_))
        ));
        Ok(())
    }

    #[test]
    fn for_launch_never_falls_back() -> anyhow::Result<()> {
        let dir = private_dir()?;
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked)?;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))?;
        let path = locked.join("session_worker.secret");
        let generated = std::sync::atomic::AtomicBool::new(false);
        let outcome = for_launch_at(&path, || {
            generated.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok([9u8; 32])
        });
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))?;
        assert!(
            matches!(outcome, Err(TransportSecretError::TransportSecretUnavailable(_))),
            "an unwritable directory refuses: {outcome:?}"
        );
        assert!(!path.exists(), "no secret and no seal are produced");

        let widened = dir.path().join("widened");
        std::fs::create_dir(&widened)?;
        std::fs::set_permissions(&widened, std::fs::Permissions::from_mode(0o755))?;
        assert!(matches!(
            for_launch_at(&widened.join("session_worker.secret"), || Ok([9u8; 32])),
            Err(TransportSecretError::TransportSecretUnsafe(_))
        ));

        let invalid = dir.path().join("invalid.secret");
        std::fs::write(&invalid, b"short")?;
        std::fs::set_permissions(&invalid, std::fs::Permissions::from_mode(0o600))?;
        assert!(for_launch_at(&invalid, || Ok([9u8; 32])).is_err());
        assert_eq!(
            std::fs::read(&invalid)?,
            b"short",
            "an invalid secret is never replaced"
        );
        Ok(())
    }

    #[test]
    fn concurrent_for_launch_converges() -> anyhow::Result<()> {
        let dir = private_dir()?;
        let path = dir.path().join("keys").join("session_worker.secret");
        let ready = std::sync::Barrier::new(2);
        let (first, second) = std::thread::scope(|scope| -> anyhow::Result<_> {
            let first = scope.spawn(|| {
                for_launch_at(&path, || {
                    ready.wait();
                    Ok([1u8; 32])
                })
            });
            let second = scope.spawn(|| {
                for_launch_at(&path, || {
                    ready.wait();
                    Ok([2u8; 32])
                })
            });
            Ok((
                first.join().map_err(|_| anyhow::anyhow!("first creator panicked"))??,
                second
                    .join()
                    .map_err(|_| anyhow::anyhow!("second creator panicked"))??,
            ))
        })?;
        assert_eq!(first.0, second.0);
        assert_eq!(existing_at(&path)?.0, first.0);
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }
}
