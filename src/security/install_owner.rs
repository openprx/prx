//! The install owner id (HOST-1a, `owner_id_stable_v1`).
//!
//! One random id per state directory, `api:install:<32 lowercase hex>`, kept
//! at `<workspace>/self/runtime/install_owner` (mode `0600`) beside the
//! runtime key, under the same directory checks. Every paired bearer of the
//! gateway acts for this one owner, so bearer rotation, re-pairing, restarts
//! and further paired devices all keep the same owner.
//!
//! The id is not a secret (it may be logged), but it is integrity-relevant:
//! a different id leaves every owner-keyed row unreachable. A file that is
//! malformed, not a regular file, open to others or owned by another user
//! therefore refuses gateway start-up and is never replaced. Only a missing
//! file is created, atomically: a private temporary file is linked into
//! place, so the file is never half-written, never readable by others, and a
//! concurrent second start reads the id the first one linked.

use std::fmt;
use std::path::{Path, PathBuf};

/// The file name inside the runtime key's directory.
const FILE_NAME: &str = "install_owner";

/// The id prefix. It keeps `api:` so the H2 identity profile and its
/// evidence path accept it.
const PREFIX: &str = "api:install:";

/// Lowercase hex digits after the prefix (128 random bits).
const HEX_LEN: usize = 32;

/// The exact length of the file: the id, with no newline.
const ID_LEN: usize = PREFIX.len() + HEX_LEN;

/// The install owner id of this state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOwnerId(String);

impl InstallOwnerId {
    /// The id as stored and as every owner-keyed row carries it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id the file holds, or `None` when the bytes are not exactly
    /// `api:install:` and 32 lowercase hex digits.
    fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let hex = text.strip_prefix(PREFIX)?;
        (hex.len() == HEX_LEN && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(text.to_owned()))
    }

    /// A new id from the OS CSPRNG.
    fn generate() -> Self {
        let bytes: [u8; HEX_LEN / 2] = rand::random();
        Self(format!("{PREFIX}{}", hex::encode(bytes)))
    }

    /// A fixed id for gateway test fixtures; never written to disk.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self(format!("{PREFIX}{}", "0".repeat(HEX_LEN)))
    }
}

impl fmt::Display for InstallOwnerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why the install owner id could not be loaded or created. Every variant
/// refuses gateway start-up. No variant carries the file's content.
#[derive(Debug, thiserror::Error)]
pub enum InstallOwnerError {
    #[error("the install owner file {path} cannot be used: its directory is unsafe or cannot be created: {reason}")]
    Directory { path: PathBuf, reason: String },
    #[error("the install owner file {path} is not a regular file (a symlink or special file); it is never replaced")]
    NotRegular { path: PathBuf },
    #[error(
        "the install owner file {path} has mode {mode:o}; it must be readable and writable by its owner only (0600). It is never regenerated: run `chmod 600` on it"
    )]
    Permissions { path: PathBuf, mode: u32 },
    #[error("the install owner file {path} is owned by another user; it is never regenerated")]
    ForeignOwner { path: PathBuf },
    #[error(
        "the install owner file {path} is malformed: it must hold exactly `api:install:` and 32 lowercase hex digits, with no newline. It is never regenerated: restore it from a backup (removing it creates a new owner and leaves every row of the old one unreachable)"
    )]
    Malformed { path: PathBuf },
    #[error("the install owner file {path} cannot be read or created: {reason}")]
    Io { path: PathBuf, reason: String },
}

/// Load the install owner id of the state directory whose workspace is
/// `workspace_dir`, creating it when the file does not exist yet.
///
/// # Errors
///
/// [`InstallOwnerError`]: an unsafe directory, a file that is not a regular
/// file, has a mode open to group or others, belongs to another user or is
/// malformed, or an I/O failure. Nothing is ever overwritten.
pub fn load_or_create(workspace_dir: &Path) -> Result<InstallOwnerId, InstallOwnerError> {
    let dir =
        super::runtime_key::prepare_decision_dir(workspace_dir).map_err(|reason| InstallOwnerError::Directory {
            path: workspace_dir.join("self").join("runtime").join(FILE_NAME),
            reason,
        })?;
    let path = dir.join(FILE_NAME);
    if let Some(existing) = read(&path)? {
        return Ok(existing);
    }
    let created = InstallOwnerId::generate();
    if link_new(&dir, &path, &created)? {
        return Ok(created);
    }
    // Another process linked its id first: that one is the install owner.
    read(&path)?.ok_or_else(|| InstallOwnerError::Io {
        path,
        reason: "the file vanished after a concurrent create".to_owned(),
    })
}

/// The id in `path`, `None` when no file exists, or why the file is refused.
/// The file is opened once without following a symlink, and every check and
/// the read use that handle; the length is checked before reading.
fn read(path: &Path) -> Result<Option<InstallOwnerId>, InstallOwnerError> {
    use std::io::Read as _;

    let io = |reason: String| InstallOwnerError::Io {
        path: path.to_path_buf(),
        reason,
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // `O_NONBLOCK` keeps a FIFO planted at the path from blocking the
        // open; the type is checked on the handle right after.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io(format!("cannot open it without following links: {error}"))),
    };
    let metadata = file
        .metadata()
        .map_err(|error| io(format!("cannot inspect it: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(InstallOwnerError::NotRegular {
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let mode = metadata.permissions().mode() & 0o7777;
        if mode & 0o077 != 0 {
            return Err(InstallOwnerError::Permissions {
                path: path.to_path_buf(),
                mode,
            });
        }
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(InstallOwnerError::ForeignOwner {
                path: path.to_path_buf(),
            });
        }
    }
    let malformed = || InstallOwnerError::Malformed {
        path: path.to_path_buf(),
    };
    if metadata.len() != ID_LEN as u64 {
        return Err(malformed());
    }
    let mut bytes = [0_u8; ID_LEN];
    file.read_exact(&mut bytes)
        .map_err(|error| io(format!("cannot read it: {error}")))?;
    InstallOwnerId::parse(&bytes).map(Some).ok_or_else(malformed)
}

/// Write `id` to a private temporary file in `dir`, then hard-link it to
/// `path`. `Ok(false)` when a file already exists there: a link never
/// overwrites, so two concurrent starts never hold two ids.
fn link_new(dir: &Path, path: &Path, id: &InstallOwnerId) -> Result<bool, InstallOwnerError> {
    let temp = dir.join(format!(".install_owner.tmp-{}", uuid::Uuid::new_v4()));
    let linked = super::runtime_key::write_private_file(&temp, id.as_str().as_bytes()).and_then(|()| {
        match std::fs::hard_link(&temp, path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(format!("cannot link it into place: {error}")),
        }
    });
    if let Err(error) = std::fs::remove_file(&temp) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %temp.display(), %error, "could not remove the temporary install owner file");
        }
    }
    let io = |reason: String| InstallOwnerError::Io {
        path: path.to_path_buf(),
        reason,
    };
    let linked = linked.map_err(io)?;
    if linked {
        super::runtime_key::sync_dir(dir).map_err(io)?;
    }
    Ok(linked)
}
